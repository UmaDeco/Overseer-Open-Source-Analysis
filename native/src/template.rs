//! Localization templating engine (ported from the upstream localization framework's template engine + filters).
//!
//! Localized strings may contain `$(filter arg1 arg2 …)` expressions (Bash-substitution-like).
//! `eval()` expands them. Built-in filters, reading the active pack's `LocalizedData`:
//!   * `$(plural n 'form0' 'form1' …)` — pick a plural form by the pack's `plural_form` rule; `$` in
//!     the chosen form is replaced with `n`.
//!   * `$(ordinal n)` — ordinal suffix via `ordinal_form` + `config.ordinal_types`.
//!   * `$(month n)` — `config.months[n-1]`.
//! Every localization hook (Localize::Get, SQLite GetText, story) runs its dict text through `eval`.

#![allow(dead_code)]

use fnv::FnvHashMap;
use once_cell::sync::Lazy;

pub enum Token {
    Identifier(String),
    NumberLit(f64),
    StringLit(String),
}

pub type Filter = fn(args: &[Token]) -> Option<String>;

pub trait Context {
    fn on_filter_eval(&mut self, name: &str, args: &[Token]) -> Option<String>;
}

struct EmptyContext;
impl Context for EmptyContext {
    fn on_filter_eval(&mut self, _name: &str, _args: &[Token]) -> Option<String> {
        None
    }
}

struct FilterRemovalContext;
impl Context for FilterRemovalContext {
    fn on_filter_eval(&mut self, _name: &str, _args: &[Token]) -> Option<String> {
        Some(String::new())
    }
}

fn log(m: &str) {
    crate::tools::log(m);
}

pub struct Parser {
    filters: FnvHashMap<String, Filter>,
}

impl Parser {
    pub fn new(filters_: &[(&str, Filter)]) -> Parser {
        let mut filters = FnvHashMap::default();
        for (name, filter) in filters_ {
            filters.insert(name.to_string(), *filter);
        }
        Parser { filters }
    }

    fn eval_filter(&self, tokens: &[Token], context: &mut impl Context) -> Option<String> {
        if tokens.is_empty() {
            return None;
        }
        if let Token::Identifier(filter_name) = &tokens[0] {
            let args = &tokens[1..];
            let context_res = context.on_filter_eval(filter_name, args);
            if context_res.is_some() {
                return context_res;
            } else if let Some(filter) = self.filters.get(filter_name) {
                return filter(&tokens[1..]);
            }
        }
        None
    }

    fn parse_token(input: &str) -> Option<Token> {
        let mut iter = input.chars();
        let start_char = iter.next().unwrap(); // guaranteed >=1 char
        let end_char = iter.last();

        if start_char == '\'' && end_char == Some('\'') {
            return Some(Token::StringLit(input[1..input.len() - 1].replace("\\'", "'")));
        }
        if start_char.is_numeric() {
            return if let Ok(number) = input.parse::<f64>() {
                Some(Token::NumberLit(number))
            } else if let Ok(number) = input.replace(',', "").parse::<f64>() {
                Some(Token::NumberLit(number))
            } else {
                None
            };
        }
        if input.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Some(Token::Identifier(input.to_owned()));
        }
        None
    }

    pub fn eval(&self, input: &str) -> String {
        self.eval_with_context(input, &mut EmptyContext)
    }

    pub fn eval_with_context(&self, input: &str, context: &mut impl Context) -> String {
        let mut output: Vec<u8> = Vec::with_capacity(input.len());

        let mut start_expr = false;
        let mut in_filter = false;
        let mut checkpoint: usize = 0;
        let mut tokens: Vec<Token> = Vec::new();
        let mut token_start: usize = 0;
        let mut in_string = false;
        let mut start_escape = false;

        // Byte iteration is fine: UTF-8 continuation bytes never collide with the ASCII syntax chars.
        for (i, c) in input.bytes().enumerate() {
            output.push(c);

            if in_filter {
                if start_escape {
                    start_escape = false;
                    continue;
                }
                match c {
                    b')' => 'filter_close: {
                        if in_string {
                            break 'filter_close;
                        }
                        if token_start != 0 {
                            match Self::parse_token(&input[token_start..i]) {
                                Some(token) => {
                                    tokens.push(token);
                                    token_start = 0;
                                }
                                None => {
                                    log(&format!("[template] invalid token in '{input}' (pos {token_start})"));
                                    token_start = 0;
                                    tokens.clear();
                                    in_filter = false;
                                    break 'filter_close;
                                }
                            }
                        }
                        if let Some(res) = self.eval_filter(&tokens, context) {
                            output.truncate(checkpoint);
                            output.extend(res.bytes());
                        } else {
                            log(&format!("[template] filter eval failed in '{input}' (pos {i})"));
                        }
                        tokens.clear();
                        in_filter = false;
                    }
                    b' ' => {
                        if !in_string && token_start != 0 {
                            match Self::parse_token(&input[token_start..i]) {
                                Some(token) => tokens.push(token),
                                None => {
                                    log(&format!("[template] invalid token in '{input}' (pos {token_start})"));
                                    tokens.clear();
                                    in_filter = false;
                                }
                            }
                            token_start = 0;
                        }
                    }
                    b'\'' => {
                        if token_start == 0 {
                            token_start = i;
                            in_string = true;
                        } else {
                            in_string = false;
                        }
                    }
                    b'\\' => {
                        if in_string {
                            start_escape = true;
                        }
                    }
                    _ => {
                        if token_start == 0 {
                            token_start = i;
                        }
                    }
                }
                continue;
            }

            if start_expr {
                if c == b'(' {
                    in_filter = true;
                }
                start_expr = false;
                continue;
            }

            if c == b'$' {
                start_expr = true;
                checkpoint = output.len() - 1; // before the '$'
            }
        }

        // output only ever contains the original bytes minus/plus valid UTF-8 filter results.
        String::from_utf8(output).unwrap_or_else(|_| input.to_string())
    }

    /// Evaluate with a context that erases every filter expression.
    pub fn remove_filters(&self, input: &str) -> String {
        self.eval_with_context(input, &mut FilterRemovalContext)
    }
}

// ── built-in filters (read the active pack) ──────────────────────────────────
static PARSER: Lazy<Parser> = Lazy::new(|| Parser::new(&FILTERS));

static FILTERS: [(&str, Filter); 3] = [("plural", plural), ("ordinal", ordinal), ("month", month)];

/// Expand `$(...)` filter expressions in a localized string. Cheap no-op if there is no `$`.
pub fn eval(input: &str) -> String {
    if !input.as_bytes().contains(&b'$') {
        return input.to_string();
    }
    PARSER.eval(input)
}

/// `$(plural n 'plural_type_0' 'plural_type_1' ...)`
fn plural(args: &[Token]) -> Option<String> {
    if args.len() < 2 {
        return None;
    }
    if let Token::NumberLit(n) = args[0] {
        let ld = crate::localize::data();
        let plural_type = 1 + ld.plural_form.resolve(n as u64);
        if let Some(Token::StringLit(s)) = args.get(plural_type) {
            return Some(s.replace('$', &n.to_string()));
        }
    }
    None
}

/// `$(ordinal n)`
fn ordinal(args: &[Token]) -> Option<String> {
    if let Some(Token::NumberLit(n)) = args.first() {
        let ld = crate::localize::data();
        let i = ld.ordinal_form.resolve(*n as u64);
        let ordinal_type = ld.config.ordinal_types.get(i)?;
        return Some(ordinal_type.replace('$', &n.to_string()));
    }
    None
}

/// `$(month n)`
fn month(args: &[Token]) -> Option<String> {
    if let Some(Token::NumberLit(i)) = args.first() {
        let ld = crate::localize::data();
        return ld.config.months.get((*i as usize).saturating_sub(1)).cloned();
    }
    None
}
