// Vue Template Parser does not adhere to HTML spec.
// https://html.spec.whatwg.org/multipage/parsing.html#tree-construction
// According to the spec: tree construction has several points:
// 1. Tree Construction Dispatcher: N/A. We don't consider foreign content.
// 2. appropriate place for inserting a node: For table/template elements.
//    N/A.  We can't know the global tree in a component.
// 3. create an element for a token: For custom component
//    N/A. We don't handle JS execution for custom component.
// 4. adjust MathML/SVG attributes:
//    ?? Should we handle this? The original Vue compiler does not.
// 5. Inserting Text/Comment: N/A. We don't handle script/insertion location.
// 6. Parsing elements that contain only text: Already handled in scanner.
// 7. Closing elements that have implied end tags:
//    N/A: Rule is too complicated and requires non-local context.
// Instead, we use a simple stack to construct AST.

use super::{
    error::{CompilationError, CompilationErrorKind as ErrorKind, RcErrHandle},
    flags::RuntimeHelper,
    scanner::{Attribute, AttributeValue, Tag, TextMode, Token, TokenSource},
    util::{find_dir, is_core_component, no, non_whitespace, yes, VStr},
    Name, Namespace, SourceLocation,
};
use smallvec::{smallvec, SmallVec};
use std::ops::Deref;

#[cfg(feature = "serde")]
use serde::Serialize;

#[cfg_attr(feature = "serde", derive(Serialize))]
pub enum AstNode<'a> {
    Element(Element<'a>),
    Text(TextNode<'a>),
    Interpolation(SourceNode<'a>),
    Comment(SourceNode<'a>),
}

impl<'a> AstNode<'a> {
    pub fn get_element(&self) -> Option<&Element<'a>> {
        match self {
            AstNode::Element(e) => Some(e),
            _ => None,
        }
    }
    pub fn get_element_mut(&mut self) -> Option<&mut Element<'a>> {
        match self {
            AstNode::Element(e) => Some(e),
            _ => None,
        }
    }
    pub fn into_element(self) -> Element<'a> {
        match self {
            AstNode::Element(e) => e,
            _ => panic!("call into_element on non-element AstNode"),
        }
    }
    pub fn get_location(&self) -> &SourceLocation {
        match self {
            Self::Element(e) => &e.location,
            Self::Text(t) => &t.location,
            Self::Interpolation(i) => &i.location,
            Self::Comment(c) => &c.location,
        }
    }
}

#[cfg_attr(feature = "serde", derive(Serialize))]
pub struct SourceNode<'a> {
    pub source: &'a str,
    pub location: SourceLocation,
}

pub struct TextNode<'a> {
    pub text: SmallVec<[VStr<'a>; 1]>,
    pub location: SourceLocation,
}
#[cfg(feature = "serde")]
impl<'a> Serialize for TextNode<'a> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("TextNode", 2)?;
        let s = self.text.iter().map(|&s| s.into_string());
        let s: String = s.collect();
        state.serialize_field("text", &s)?;
        state.serialize_field("location", &self.location)?;
        state.end()
    }
}

impl<'a> Deref for TextNode<'a> {
    type Target = str;
    fn deref(&self) -> &Self::Target {
        debug_assert!(self.text.len() == 1);
        &self.text[0]
    }
}

impl<'a> TextNode<'a> {
    /// if TextNode contains only whitespaces. In HTML it means empty node.
    pub fn is_all_whitespace(&self) -> bool {
        self.text.iter().all(|s| !s.chars().any(non_whitespace))
    }
    pub fn trim_leading_newline(&mut self) {
        if self.text.is_empty() {
            return;
        }
        let first = &self.text[0];
        let offset = if first.starts_with('\n') {
            1
        } else if first.starts_with("\r\n") {
            2
        } else {
            return;
        };
        if first.len() > offset {
            self.text[0] = VStr {
                raw: &first.raw[offset..],
                ops: first.ops,
            };
        } else {
            self.text.remove(0);
        }
    }
}

#[cfg_attr(feature = "serde", derive(Serialize))]
pub enum ElemProp<'a> {
    Attr(Attribute<'a>),
    Dir(Directive<'a>),
}

impl<'a> ElemProp<'a> {
    pub fn get_location(&self) -> &SourceLocation {
        match self {
            Self::Attr(a) => &a.location,
            Self::Dir(d) => &d.location,
        }
    }
    fn attr(mut a: Attribute<'a>) -> Self {
        if let Some(val) = a.value.as_mut() {
            val.content.decode(true);
        }
        Self::Attr(a)
    }
}

#[derive(PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize))]
pub enum ElementType {
    Plain,
    Component,
    Template,
    SlotOutlet,
}

#[cfg_attr(feature = "serde", derive(Serialize))]
pub struct Element<'a> {
    pub tag_name: Name<'a>,
    pub tag_type: ElementType,
    pub namespace: Namespace,
    pub properties: Vec<ElemProp<'a>>,
    pub children: Vec<AstNode<'a>>,
    pub location: SourceLocation,
}

impl<'a> Element<'a> {
    #[inline]
    pub fn is_component(&self) -> bool {
        self.tag_type == ElementType::Component
    }
}

/// Directive supports two forms
/// static and dynamic
#[cfg_attr(feature = "serde", derive(Serialize))]
pub enum DirectiveArg<'a> {
    // :static="val"
    Static(Name<'a>),
    Dynamic(Name<'a>), // :[dynamic]="val"
}

/// Directive has the form
/// v-name:arg.mod1.mod2="expr"
#[derive(Default)]
#[cfg_attr(feature = "serde", derive(Serialize))]
pub struct Directive<'a> {
    pub name: &'a str,
    pub argument: Option<DirectiveArg<'a>>,
    pub modifiers: Vec<&'a str>,
    pub expression: Option<AttributeValue<'a>>,
    pub head_loc: SourceLocation,
    pub location: SourceLocation,
}

impl<'a> Directive<'a> {
    pub fn has_empty_expr(&self) -> bool {
        self.expression
            .as_ref()
            .map_or(true, |v| !v.content.contains(non_whitespace))
    }
    /// Returns the error if expression is empty
    pub fn check_empty_expr(&self, kind: ErrorKind) -> Option<CompilationError> {
        if !self.has_empty_expr() {
            return None;
        }
        let loc = self
            .expression
            .as_ref()
            .map_or(self.head_loc.clone(), |v| v.location.clone());
        Some(CompilationError::new(kind).with_location(loc))
    }
}

#[cfg_attr(feature = "serde", derive(Serialize))]
pub struct AstRoot<'a> {
    pub children: Vec<AstNode<'a>>,
    pub location: SourceLocation,
}

#[derive(Clone, Default)]
pub enum WhitespaceStrategy {
    Preserve,
    #[default]
    Condense,
}

// `is_xxx` methods in ParseOption targets different audience.
// Please refer to project README for more details.
#[derive(Clone)]
pub struct ParseOption {
    pub whitespace: WhitespaceStrategy,
    pub preserve_comment: bool,
    pub get_namespace: fn(&str, Option<&Element<'_>>) -> Namespace,
    pub get_text_mode: fn(&str) -> TextMode,
    /// Returns if a tag is self closing.
    pub is_void_tag: fn(&str) -> bool,
    // probably we don't need configure pre tag?
    // in original Vue this is only used for parsing SFC.
    pub is_pre_tag: fn(&str) -> bool,
    /// Exposed to end user for customization like importing web-component from React.
    pub is_custom_element: fn(&str) -> bool,
    /// For platform developers. Registers platform specific components written in JS.
    /// e.g. transition, transition-group. Components that require code in Vue runtime.
    pub get_builtin_component: fn(&str) -> Option<RuntimeHelper>,
    /// For platform developer. Registers platform components written in host language like C++.
    pub is_native_element: fn(&str) -> bool,
}

impl Default for ParseOption {
    fn default() -> Self {
        Self {
            whitespace: WhitespaceStrategy::Condense,
            preserve_comment: true,
            get_namespace: |_, _| Namespace::Html,
            get_text_mode: |_| TextMode::Data,
            is_void_tag: no,
            is_pre_tag: |s| s == "pre",
            is_custom_element: no,
            get_builtin_component: |_| None,
            is_native_element: yes,
        }
    }
}

pub struct Parser {
    option: ParseOption,
}

impl Parser {
    pub fn new(option: ParseOption) -> Self {
        Self { option }
    }

    pub fn parse<'a, Ts>(&self, tokens: Ts, err_handle: RcErrHandle) -> AstRoot<'a>
    where
        Ts: TokenSource<'a>,
    {
        let need_flag_namespace = tokens.need_flag_hint();
        AstBuilder {
            tokens,
            err_handle,
            option: self.option.clone(),
            open_elems: vec![],
            root_nodes: vec![],
            pre_count: 0,
            v_pre_index: None,
            need_flag_namespace,
        }
        .build_ast()
    }
}

// TODO: remove Eh as generic
struct AstBuilder<'a, Ts>
where
    Ts: TokenSource<'a>,
{
    tokens: Ts,
    err_handle: RcErrHandle,
    option: ParseOption,
    open_elems: Vec<Element<'a>>,
    root_nodes: Vec<AstNode<'a>>,
    // how many <pre> already met
    pre_count: usize,
    // the idx of v-pre boundary in open_elems
    // NB: idx is enough since v-pre does not nest
    v_pre_index: Option<usize>,
    need_flag_namespace: bool,
}

// utility method
impl<'a, Ts> AstBuilder<'a, Ts>
where
    Ts: TokenSource<'a>,
{
    // Insert node into current insertion point.
    // It's the last open element's children if open_elems is not empty.
    // Otherwise it is root_nodes.
    fn insert_node(&mut self, node: AstNode<'a>) {
        if let Some(elem) = self.open_elems.last_mut() {
            elem.children.push(node);
        } else {
            self.root_nodes.push(node);
        }
    }

    fn emit_error(&self, kind: ErrorKind, loc: SourceLocation) {
        let error = CompilationError::new(kind).with_location(loc);
        self.err_handle.on_error(error)
    }
}

// parse logic
impl<'a, Ts> AstBuilder<'a, Ts>
where
    Ts: TokenSource<'a>,
{
    fn build_ast(mut self) -> AstRoot<'a> {
        let start = self.tokens.current_position();
        while let Some(token) = self.tokens.next() {
            self.parse_token(token);
        }
        self.report_unclosed_script_comment();
        for _ in 0..self.open_elems.len() {
            self.close_element(/*has_matched_end*/ false);
        }
        debug_assert_eq!(self.pre_count, 0);
        debug_assert!(self.v_pre_index.is_none());
        let need_condense = self.need_condense();
        compress_whitespaces(&mut self.root_nodes, need_condense);
        let location = self.tokens.get_location_from(start);
        AstRoot {
            children: self.root_nodes,
            location,
        }
    }

    fn parse_token(&mut self, token: Token<'a>) {
        // https://html.spec.whatwg.org/multipage/parsing.html#parsing-main-inbody:current-node-26
        match token {
            Token::EndTag(s) => self.parse_end_tag(s),
            Token::Text(text) => self.parse_text(text),
            Token::StartTag(tag) => self.parse_open_tag(tag),
            Token::Comment(c) => self.parse_comment(c),
            Token::Interpolation(i) => self.parse_interpolation(i),
        };
    }
    fn parse_open_tag(&mut self, tag: Tag<'a>) {
        let Tag {
            name,
            self_closing,
            attributes,
        } = tag;
        let props = self.parse_attributes(attributes);
        let ns = (self.option.get_namespace)(name, self.open_elems.last());
        let elem = Element {
            tag_name: name,
            tag_type: ElementType::Plain,
            namespace: ns,
            properties: props,
            children: vec![],
            location: SourceLocation {
                start: self.tokens.last_position(),
                end: self.tokens.current_position(),
            },
        };
        if self_closing || (self.option.is_void_tag)(name) {
            let node = self.parse_element(elem);
            self.insert_node(node);
        } else {
            // only element with childen needs set pre/v-pre.
            // self-closing element cancels out pre itself.
            self.handle_pre_like(&elem);
            self.open_elems.push(elem);
            self.set_scanner_flag();
        }
    }
    fn parse_attributes(&mut self, mut attrs: Vec<Attribute<'a>>) -> Vec<ElemProp<'a>> {
        // in v-pre, parse no directive
        if self.v_pre_index.is_some() {
            return attrs.into_iter().map(ElemProp::Attr).collect();
        }
        let mut dir_parser = DirectiveParser::new(&self.err_handle);
        // v-pre precedes any other directives
        for i in 0..attrs.len() {
            if attrs[i].name != "v-pre" {
                continue;
            }
            let dir = dir_parser.parse(attrs.remove(i));
            let mut ret = vec![ElemProp::Dir(dir)];
            ret.extend(attrs.into_iter().map(ElemProp::attr));
            return ret;
        }
        attrs
            .into_iter()
            .map(|attr| {
                if dir_parser.detect_directive(&attr) {
                    // TODO: report duplicate prop by is_mergeable_prop
                    ElemProp::Dir(dir_parser.parse(attr))
                } else {
                    ElemProp::attr(attr)
                }
            })
            .collect()
    }

    fn handle_pre_like(&mut self, elem: &Element) {
        debug_assert!(
            self.open_elems
                .last()
                .map_or(true, |e| e.location != elem.location),
            "element should not be pushed to stack yet.",
        );
        // increment_pre
        if (self.option.is_pre_tag)(elem.tag_name) {
            self.pre_count += 1;
        }
        // open_v_pre
        if is_v_pre_boundary(elem) {
            debug_assert!(self.v_pre_index.is_none());
            self.v_pre_index = Some(self.open_elems.len());
        }
    }
    fn parse_end_tag(&mut self, end_tag: &'a str) {
        // rfind is good since only mismatch will traverse stack
        let index = self
            .open_elems
            .iter()
            .enumerate()
            .rfind(|p| element_matches_end_tag(p.1, end_tag))
            .map(|p| p.0);
        if let Some(i) = index {
            let mut to_close = self.open_elems.len() - i;
            while to_close > 0 {
                to_close -= 1;
                self.close_element(to_close == 0);
            }
            debug_assert_eq!(self.open_elems.len(), i);
        } else {
            let start = self.tokens.last_position();
            let loc = self.tokens.get_location_from(start);
            self.emit_error(ErrorKind::InvalidEndTag, loc);
        }
    }
    fn close_element(&mut self, has_matched_end: bool) {
        let mut elem = self.open_elems.pop().unwrap();
        self.set_scanner_flag();
        let start = elem.location.start;
        if !has_matched_end {
            // should only span the start of a tag, not the whole tag.
            let err_location = SourceLocation {
                start: start.clone(),
                end: start.clone(),
            };
            self.emit_error(ErrorKind::MissingEndTag, err_location);
        }
        let location = self.tokens.get_location_from(start);
        elem.location = location;
        if self.pre_count > 0 {
            self.decrement_pre(&mut elem)
        } else if (self.option.get_text_mode)(elem.tag_name) == TextMode::Data {
            // skip compress in pre or RAWTEXT/RCDATA
            compress_whitespaces(&mut elem.children, self.need_condense());
        }
        let node = self.parse_element(elem);
        self.insert_node(node);
    }
    fn decrement_pre(&mut self, elem: &mut Element) {
        debug_assert!(self.pre_count > 0);
        let pre_boundary = (self.option.is_pre_tag)(elem.tag_name);
        // trim pre tag's leading new line
        // https://html.spec.whatwg.org/multipage/syntax.html#element-restrictions
        if !pre_boundary {
            return;
        }
        if let Some(AstNode::Text(tn)) = elem.children.last_mut() {
            tn.trim_leading_newline();
        }
        self.pre_count -= 1;
    }
    fn close_v_pre(&mut self) {
        let idx = self.v_pre_index.unwrap();
        debug_assert!(idx <= self.open_elems.len());
        // met v-pre boundary, switch back
        if idx == self.open_elems.len() {
            self.v_pre_index = None;
        }
    }
    fn parse_element(&mut self, mut elem: Element<'a>) -> AstNode<'a> {
        debug_assert!(elem.tag_type == ElementType::Plain);
        if self.v_pre_index.is_some() {
            debug_assert!({
                let i = *self.v_pre_index.as_ref().unwrap();
                i != self.open_elems.len() || is_v_pre_boundary(&elem)
            });
            self.close_v_pre();
            elem.tag_type = ElementType::Plain;
        } else if elem.tag_name == "slot" {
            elem.tag_type = ElementType::SlotOutlet;
        } else if is_template_element(&elem) {
            elem.tag_type = ElementType::Template;
        } else if self.is_component(&elem) {
            elem.tag_type = ElementType::Component;
        }
        AstNode::Element(elem)
    }
    fn parse_text(&mut self, text: VStr<'a>) {
        let mut text = smallvec![text];
        let mut next_token = None;
        let start = self.tokens.last_position();
        for token in &mut self.tokens {
            if let Token::Text(ds) = token {
                text.push(ds);
            } else {
                next_token = Some(token);
                break;
            }
        }
        let end = self.tokens.last_position();
        let location = SourceLocation { start, end };
        let text_node = TextNode { text, location };
        self.insert_node(AstNode::Text(text_node));
        // NB: token must not be dropped
        if let Some(token) = next_token {
            self.parse_token(token);
        }
    }
    fn parse_comment(&mut self, c: &'a str) {
        // Remove comments if desired by configuration.
        if !self.option.preserve_comment {
            return;
        }
        let pos = self.tokens.last_position();
        let source_node = SourceNode {
            source: c,
            location: self.tokens.get_location_from(pos),
        };
        self.insert_node(AstNode::Comment(source_node));
    }
    fn parse_interpolation(&mut self, src: &'a str) {
        let pos = self.tokens.last_position();
        let source_node = SourceNode {
            source: src,
            location: self.tokens.get_location_from(pos),
        };
        self.insert_node(AstNode::Interpolation(source_node));
    }

    // https://html.spec.whatwg.org/multipage/parsing.html#parse-error-eof-in-script-html-comment-like-text
    fn report_unclosed_script_comment(&mut self) {
        debug_assert!(self.tokens.next().is_none());
        let elem = match self.open_elems.last() {
            Some(e) => e,
            None => return,
        };
        if !elem.tag_name.eq_ignore_ascii_case("script") {
            return;
        }
        let text = match elem.children.first() {
            Some(AstNode::Text(text)) => text,
            _ => return,
        };
        // Netscape's legacy from 1995 when JS is nascent.
        // Even 4 years before Bizarre Summer(?v=UztXN2rKQNc).
        // https://stackoverflow.com/questions/808816/
        if text.contains("<!--") && !text.contains("-->") {
            let loc = SourceLocation {
                start: self.tokens.last_position(),
                end: self.tokens.last_position(),
            };
            self.emit_error(ErrorKind::EofInScriptHtmlCommentLikeText, loc);
        }
    }

    // must call this when handle CDATA
    #[inline]
    fn set_scanner_flag(&mut self) {
        if self.need_flag_namespace {
            return;
        }
        // TODO: we can set flag only when namespace changes
        let in_html = self
            .open_elems
            .last()
            .map_or(true, |e| e.namespace == Namespace::Html);
        self.tokens.set_is_in_html(in_html)
    }

    fn is_component(&self, e: &Element) -> bool {
        let opt = &self.option;
        let tag_name = e.tag_name;
        if (opt.is_custom_element)(tag_name) {
            return false;
        }
        if tag_name == "component"
            || tag_name.starts_with(|c: char| c.is_ascii_uppercase())
            || is_core_component(tag_name)
            || (opt.get_builtin_component)(tag_name).is_some()
            || !(opt.is_native_element)(tag_name)
        {
            return true;
        }
        e.properties.iter().any(|prop| match prop {
            ElemProp::Dir(Directive { name: "is", .. }) => true,
            ElemProp::Attr(Attribute {
                name: "is",
                value: Some(v),
                ..
            }) => v.content.starts_with("vue:"),
            _ => false,
        })
    }

    fn need_condense(&self) -> bool {
        matches!(self.option.whitespace, WhitespaceStrategy::Condense)
    }
}

const BIND_CHAR: char = ':';
const MOD_CHAR: char = '.';
const ON_CHAR: char = '@';
const SLOT_CHAR: char = '#';
const SEP_BYTES: &[u8] = &[BIND_CHAR as u8, MOD_CHAR as u8];
const SHORTHANDS: &[char] = &[BIND_CHAR, ON_CHAR, SLOT_CHAR, MOD_CHAR];
const DIR_MARK: &str = "v-";

type StrPair<'a> = (&'a str, &'a str);
struct DirectiveParser<'a, 'b> {
    eh: &'b RcErrHandle,
    name_loc: SourceLocation,
    location: SourceLocation,
    cached: Option<StrPair<'a>>,
}
impl<'a, 'b> DirectiveParser<'a, 'b> {
    fn new(eh: &'b RcErrHandle) -> Self {
        Self {
            eh,
            name_loc: Default::default(),
            location: Default::default(),
            cached: None,
        }
    }
    fn attr_name_err(&self, kind: ErrorKind) {
        let error = CompilationError::new(kind).with_location(self.name_loc.clone());
        self.eh.on_error(error);
    }
    fn detect_directive(&mut self, attr: &Attribute<'a>) -> bool {
        debug_assert!(self.cached.is_none());
        self.cached = self.detect_dir_name(attr);
        self.cached.is_some()
    }
    fn set_location(&mut self, attr: &Attribute<'a>) {
        self.location = attr.location.clone();
        self.name_loc = attr.name_loc.clone();
    }

    fn parse(&mut self, attr: Attribute<'a>) -> Directive<'a> {
        let (name, prefixed) = self
            .cached
            .or_else(|| self.detect_dir_name(&attr))
            .expect("Parse without detection requires attribute be directive.");
        let is_prop = attr.name.starts_with('.');
        let is_v_slot = name == "slot";
        let (arg_str, mods_str) = self.split_arg_and_mods(prefixed, is_v_slot, is_prop);
        let argument = self.parse_directive_arg(arg_str);
        let modifiers = self.parse_directive_mods(mods_str, is_prop);
        self.cached = None; // cleanup
        let expression = Self::trim_attr_value(attr.value);
        Directive {
            name,
            argument,
            modifiers,
            expression,
            head_loc: attr.name_loc,
            location: attr.location,
        }
    }
    // NB: this function sets self's location so it's mut.
    fn detect_dir_name(&mut self, attr: &Attribute<'a>) -> Option<StrPair<'a>> {
        self.set_location(attr);
        self.parse_dir_name(attr)
    }
    // Returns the directive name and shorthand-prefixed arg/mod str, if any.
    fn parse_dir_name(&self, attr: &Attribute<'a>) -> Option<StrPair<'a>> {
        let name = attr.name;
        if !name.starts_with(DIR_MARK) {
            let ret = match name.chars().next()? {
                BIND_CHAR | MOD_CHAR => "bind",
                ON_CHAR => "on",
                SLOT_CHAR => "slot",
                _ => return None,
            };
            return Some((ret, name));
        }
        let n = &name[2..];
        let ret = n
            .bytes()
            .position(|c| SEP_BYTES.contains(&c))
            .map(|i| n.split_at(i))
            .unwrap_or((n, ""));
        if ret.0.is_empty() {
            self.attr_name_err(ErrorKind::MissingDirectiveName);
            return None;
        }
        Some(ret)
    }
    // Returns arg without shorthand/separator and dot-leading mods
    fn split_arg_and_mods(&self, prefixed: &'a str, is_v_slot: bool, is_prop: bool) -> StrPair<'a> {
        // prefixed should either be empty or starts with shorthand.
        debug_assert!(prefixed.is_empty() || prefixed.starts_with(SHORTHANDS));
        if prefixed.is_empty() {
            return ("", "");
        }
        if prefixed.len() == 1 {
            self.attr_name_err(ErrorKind::MissingDirectiveArg);
            return ("", "");
        }
        let remain = &prefixed[1..];
        // bind/on/customDir accept arg, mod. slot accepts nothing.
        // see vuejs/vue-next#1241 special case for v-slot
        if is_v_slot {
            if prefixed.starts_with(MOD_CHAR) {
                // only . can end dir_name, e.g. v-slot.error
                self.attr_name_err(ErrorKind::InvalidVSlotModifier);
                ("", prefixed)
            } else {
                debug_assert!(prefixed.starts_with(&[SLOT_CHAR, BIND_CHAR][..]));
                (remain, "")
            }
        } else if prefixed.starts_with(MOD_CHAR) && !is_prop {
            // handle v-dir.arg, only .prop expect argument
            ("", prefixed)
        } else if remain.starts_with('[') {
            self.split_dynamic_arg(remain)
        } else {
            debug_assert!(!prefixed.starts_with(SLOT_CHAR));
            // handle .prop shorthand elsewhere
            remain
                .bytes()
                .position(|u| u == MOD_CHAR as u8)
                .map(|i| remain.split_at(i))
                .unwrap_or((remain, ""))
        }
    }
    fn split_dynamic_arg(&self, remain: &'a str) -> (&'a str, &'a str) {
        // dynamic arg
        let bytes = remain.as_bytes();
        let end = bytes
            .iter()
            .position(|b| *b == b']')
            .map_or(bytes.len(), |i| i + 1);
        let (arg, mut mods) = remain.split_at(end);
        if mods.starts_with(|c| c != MOD_CHAR) {
            self.attr_name_err(ErrorKind::UnexpectedContentAfterDynamicDirective);
            mods = mods.trim_start_matches(|c| c != MOD_CHAR);
        }
        (arg, mods)
    }
    fn parse_directive_arg(&self, arg: &'a str) -> Option<DirectiveArg<'a>> {
        if arg.is_empty() {
            return None;
        }
        Some(if !arg.starts_with('[') {
            DirectiveArg::Static(arg)
        } else if let Some(i) = arg.chars().position(|c| c == ']') {
            debug_assert!(i == arg.len() - 1);
            DirectiveArg::Dynamic(&arg[1..i])
        } else {
            self.attr_name_err(ErrorKind::MissingDynamicDirectiveArgumentEnd);
            DirectiveArg::Dynamic(&arg[1..])
        })
    }
    // TODO: check duplicate modifiers
    fn parse_directive_mods(&self, mods: &'a str, is_prop: bool) -> Vec<&'a str> {
        debug_assert!(mods.is_empty() || mods.starts_with(MOD_CHAR));
        let report_missing_mod = |s: &&str| {
            if s.is_empty() {
                self.attr_name_err(ErrorKind::MissingDirectiveMod);
            }
        };
        let mut ret = if mods.is_empty() {
            vec![]
        } else {
            mods[1..]
                .as_bytes()
                .split(|b| *b == b'.')
                .map(std::str::from_utf8) // use unsafe if too slow
                .map(Result::unwrap)
                .inspect(report_missing_mod)
                .collect()
        };
        if is_prop {
            ret.push("prop")
        }
        ret
    }

    fn trim_attr_value(attr_val: Option<AttributeValue>) -> Option<AttributeValue> {
        if let Some(mut val) = attr_val {
            val.content.raw = val.content.raw.trim();
            Some(val)
        } else {
            None
        }
    }
}

fn compress_whitespaces(nodes: &mut Vec<AstNode>, need_condense: bool) {
    // no two consecutive Text node, ensured by parse_text
    debug_assert!({
        let no_consecutive_text = |last_is_text, is_text| {
            if last_is_text && is_text {
                None
            } else {
                Some(is_text)
            }
        };
        nodes
            .iter()
            .map(|n| matches!(n, AstNode::Text(_)))
            .try_fold(false, no_consecutive_text)
            .is_some()
    });
    let mut i = 0;
    while i < nodes.len() {
        let should_remove = if let AstNode::Text(child) = &nodes[i] {
            use AstNode as A;
            if !child.is_all_whitespace() {
                // non empty text node
                if need_condense {
                    compress_text_node(&mut nodes[i]);
                }
                false
            } else if i == nodes.len() - 1 || i == 0 {
                // Remove the leading/trailing whitespace
                true
            } else if !need_condense {
                false
            } else {
                // Condense mode remove whitespaces between comment and
                // whitespaces with contains newline between two elements
                let prev = &nodes[i - 1];
                let next = &nodes[i + 1];
                match (prev, next) {
                    (A::Comment(_), A::Comment(_)) => true,
                    _ => is_element(prev) && is_element(next) && child.contains(&['\r', '\n'][..]),
                }
            }
        } else {
            false
        };
        if should_remove {
            nodes.remove(i);
        } else {
            i += 1;
        }
    }
}

#[inline]
fn is_element(n: &AstNode) -> bool {
    n.get_element().is_some()
}

fn compress_text_node(n: &mut AstNode) {
    if let AstNode::Text(src) = n {
        for s in src.text.iter_mut() {
            s.compress_whitespace();
        }
    } else {
        debug_assert!(false, "impossible");
    }
}

fn is_special_template_directive(n: &str) -> bool {
    // we only have 5 elements to compare. == takes 2ns while phf takes 26ns
    match n.len() {
        2 => n == "if",
        3 => n == "for",
        4 => n == "else" || n == "slot",
        7 => n == "else-if",
        _ => false,
    }
}

fn is_template_element(e: &Element) -> bool {
    e.tag_name == "template" && find_dir(e, is_special_template_directive).is_some()
}

fn element_matches_end_tag(e: &Element, tag: &str) -> bool {
    e.tag_name.eq_ignore_ascii_case(tag)
}

fn is_v_pre_boundary(elem: &Element) -> bool {
    find_dir(elem, "pre").is_some()
}

#[cfg(test)]
pub mod test {
    use super::*;
    use crate::{cast, error::test::TestErrorHandler, scanner::test::base_scan};

    #[test]
    fn test_parse_text() {
        let case = "hello {{world}}<p/><p/>";
        let ast = base_parse(case);
        let mut children = ast.children;
        assert_eq!(children.len(), 4);
        children.pop();
        children.pop();
        let world = children.pop().unwrap();
        let hello = children.pop().unwrap();
        let v = cast!(hello, AstNode::Text);
        assert_eq!(v.text[0].raw, "hello ");
        let v = cast!(world, AstNode::Interpolation);
        assert_eq!(v.source, "world");
    }
    #[test]
    fn test_decode_attr() {
        let case = "<p decode='&amp;' />";
        let ast = base_parse(case);
        let mut children = ast.children;
        let child = children.remove(0);
        let mut p = cast!(child, AstNode::Element);
        let decode = p.properties.remove(0);
        let decode = cast!(decode, ElemProp::Attr);
        let val = decode.value.unwrap().content;
        assert_eq!(val.into_string(), "&");
    }

    pub fn base_parse(s: &str) -> AstRoot {
        let tokens = base_scan(s);
        let parser = Parser::new(ParseOption {
            is_native_element: |s| s != "comp",
            ..Default::default()
        });
        let eh = std::rc::Rc::new(TestErrorHandler);
        parser.parse(tokens, eh)
    }

    pub fn mock_element(s: &str) -> Element {
        let mut m = base_parse(s).children;
        m.pop().unwrap().into_element()
    }
}
