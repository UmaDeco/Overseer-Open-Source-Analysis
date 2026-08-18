// src/wrap.rs
//! Wave-5 shared CJK-aware WRAP/FIT text engine (pure Rust; NO il2cpp on the core path).
//! Ported verbatim from the upstream localization framework's text utilities; the ONLY adaptation
//! is the config source: the upstream singleton's `localized_data.load().config` becomes `crate::localize::data().config`
//! (identical field names + Option<f32> semantics). Public entries return None when the wrapper is
//! disabled or `line_width_multiplier` is unset, so callers fall back to the game's native text.
//!
//! Deps already in Cargo.toml with DEFAULT features (do NOT set default-features=false):
//!   textwrap = "0.16"  -> needs `smawk` (wrap_optimal_fit) + `unicode-linebreak` (UnicodeBreakProperties)
//!   unicode-width = "0.1" (truncate path only)
//!
//! `IsolateTags` is pub: `crate::loc_story` reuses it to count non-tag chars for typewrite timing.

#![allow(dead_code)]

use std::borrow::Cow;

use textwrap::{core::Word, wrap_algorithms, WordSeparator::UnicodeBreakProperties};
use unicode_width::UnicodeWidthChar; // truncate_chars path only

// ===== tag/text isolator used by custom_word_separator (and by loc_story text_len) =====
// Iterator over (&str slice, needs_break) — tag runs -> false, text runs -> true.
pub struct IsolateTags<'a> {
    s: &'a str,
    bytes: std::str::Bytes<'a>,
    i: usize,
    current_byte: Option<u8>,
}

impl<'a> IsolateTags<'a> {
    pub fn new(s: &'a str) -> Self {
        let mut bytes = s.bytes();
        Self { current_byte: bytes.next(), s, bytes, i: 0 }
    }
}

impl<'a> Iterator for IsolateTags<'a> {
    type Item = (&'a str, bool);
    fn next(&mut self) -> Option<Self::Item> {
        if self.current_byte.is_none() {
            return None;
        }
        let start = self.i;
        let mut tag_start = 0;
        let mut in_tag = false;
        let mut in_closing_tag = false;
        let mut expecting_tag_name = false;
        while let Some(c) = self.current_byte {
            if in_tag {
                match c {
                    b'>' | b'=' | b' ' => 'tag_name_end: {
                        if expecting_tag_name {
                            if !in_closing_tag {
                                let tag_name = &self.s[tag_start + 1..self.i];
                                let mut closing_tag = String::with_capacity(3 + tag_name.len());
                                closing_tag += "</";
                                closing_tag += tag_name;
                                closing_tag += ">";
                                if !self.s[self.i..].contains(&closing_tag) {
                                    in_tag = false;
                                    break 'tag_name_end;
                                }
                            }
                            expecting_tag_name = false;
                        }
                        if c == b'>' {
                            loop {
                                self.i += 1;
                                self.current_byte = self.bytes.next();
                                if let Some(c) = self.current_byte {
                                    if char::from(c).is_whitespace() {
                                        continue;
                                    }
                                }
                                break;
                            }
                            return Some((&self.s[start..self.i], false));
                        } else if in_closing_tag {
                            in_tag = false;
                        }
                    }
                    b'/' => {
                        if self.i == tag_start + 1 {
                            in_closing_tag = true;
                        } else if expecting_tag_name {
                            in_tag = false;
                        }
                    }
                    _ => {
                        if expecting_tag_name && !char::from(c).is_ascii_alphabetic() {
                            in_tag = false;
                        }
                    }
                }
            } else if c == b'<' {
                if start == self.i {
                    in_tag = true;
                    expecting_tag_name = true;
                    tag_start = self.i;
                } else {
                    break;
                }
            }
            self.i += 1;
            self.current_byte = self.bytes.next();
        }
        Some((&self.s[start..self.i], true))
    }
}

// ===== textwrap WordSeparator::Custom target (bare fn, NOT a closure) =====
fn custom_word_separator(line: &str) -> Box<dyn Iterator<Item = Word<'_>> + '_> {
    let mut isolate_iter = IsolateTags::new(line);
    let mut unicode_break_iter: Box<dyn Iterator<Item = Word<'_>> + '_> = Box::new(std::iter::empty());
    Box::new(std::iter::from_fn(move || {
        let break_res = unicode_break_iter.next();
        if break_res.is_some() {
            return break_res;
        }
        loop {
            if let Some((next_section, needs_break)) = isolate_iter.next() {
                if needs_break {
                    let mut iter = UnicodeBreakProperties.find_words(next_section);
                    let break_res = iter.next();
                    if break_res.is_some() {
                        unicode_break_iter = iter;
                        return break_res;
                    }
                } else {
                    unicode_break_iter = Box::new(std::iter::empty());
                    return Some(Word::from(next_section));
                }
            } else {
                return None;
            }
        }
    }))
}

// ===== textwrap WrapAlgorithm::Custom target (bare fn) =====
fn custom_wrap_algorithm<'a, 'b>(words: &'b [Word<'a>], line_widths: &'b [usize]) -> Vec<&'b [Word<'a>]> {
    let mut clean_fragments = Vec::with_capacity(words.len());
    let mut removed_indices = Vec::with_capacity(words.len());
    let mut remove_offset = 0;
    for (i, word) in words.iter().enumerate() {
        if word.starts_with("<") && word.ends_with(">") {
            removed_indices.push(i - remove_offset);
            remove_offset += 1;
            continue;
        }
        clean_fragments.push(words[i]);
    }
    let f64_line_widths = line_widths.iter().map(|w| *w as f64).collect::<Vec<_>>();
    if remove_offset == 0 {
        return wrap_algorithms::wrap_optimal_fit(words, &f64_line_widths, &wrap_algorithms::Penalties::new()).unwrap();
    }
    let wrapped =
        wrap_algorithms::wrap_optimal_fit(&clean_fragments, &f64_line_widths, &wrap_algorithms::Penalties::new()).unwrap();
    let mut lines = Vec::with_capacity(wrapped.len());
    let mut start = 0;
    let mut clean_start = 0;
    let mut removed_indices_i = 0;
    for (i, line) in wrapped.iter().enumerate() {
        let mut end: usize;
        if i == wrapped.len() - 1 {
            end = words.len();
        } else {
            let clean_end = clean_start + line.len();
            end = start + line.len();
            loop {
                let Some(index) = removed_indices.get(removed_indices_i) else { break; };
                if *index >= clean_start {
                    if *index < clean_end {
                        end += 1;
                        removed_indices_i += 1;
                    } else {
                        break;
                    }
                }
            }
            clean_start = clean_end;
        }
        lines.push(&words[start..end]);
        start = end;
    }
    lines
}

// ===== public entries (config source = crate::localize::data().config) =====
pub fn wrap_text(string: &str, base_line_width: i32) -> Option<Vec<Cow<'_, str>>> {
    let ld = crate::localize::data();
    let config = &ld.config;
    if !config.use_text_wrapper {
        return None;
    }
    Some(wrap_text_internal(string, base_line_width, config.line_width_multiplier?))
}

fn wrap_text_internal(string: &str, base_line_width: i32, line_width_multiplier: f32) -> Vec<Cow<'_, str>> {
    let line_width = (base_line_width as f32 * line_width_multiplier).round() as usize;
    let options = textwrap::Options::new(line_width)
        .word_separator(textwrap::WordSeparator::Custom(custom_word_separator))
        .wrap_algorithm(textwrap::WrapAlgorithm::Custom(custom_wrap_algorithm));
    textwrap::wrap(string, &options)
}

pub fn add_size_tag(string: &str, size: i32) -> String {
    let mut new_str = String::with_capacity(9 + string.len() + 7);
    new_str.push_str("<size=");
    new_str.push_str(&size.to_string());
    new_str.push('>');
    new_str.push_str(string);
    new_str.push_str("</size>");
    new_str
}

fn fit_text_internal(string: &str, base_line_width: i32, base_font_size: i32, line_width_multiplier: f32) -> Option<String> {
    let line_width = base_line_width as f32 * line_width_multiplier;
    let count = string.chars().count() as f32;
    if line_width < count {
        Some(add_size_tag(string, (base_font_size as f32 * (line_width / count)) as i32))
    } else {
        None
    }
}

// WRAP IT TILL IT FITS (brute force)
pub fn wrap_fit_text(string: &str, base_line_width: i32, mut max_line_count: i32, base_font_size: i32) -> Option<String> {
    let ld = crate::localize::data();
    let config = &ld.config;
    if !config.use_text_wrapper {
        return None;
    }
    let line_width_multiplier = config.line_width_multiplier?;
    if string.contains("<size=") {
        return None;
    }
    let mut line_width = base_line_width as f32;
    let mut font_size = base_font_size as f32;
    loop {
        let wrapped = wrap_text_internal(string, line_width.round() as i32, line_width_multiplier);
        if wrapped.len() as i32 <= max_line_count {
            return Some(add_size_tag(&wrapped.join("\n"), font_size.round() as i32));
        }
        let prev_max_line_count = max_line_count;
        max_line_count += 1;
        let scale = prev_max_line_count as f32 / max_line_count as f32;
        font_size *= scale;
        line_width /= scale;
    }
}

// TRUNCATE path (kept for later waves; only place that uses unicode-width directly).
pub fn truncate_chars(string: &str, width: i32, ellipsis: bool) -> Option<String> {
    let ld = crate::localize::data();
    let config = &ld.config;
    if !config.use_text_wrapper {
        return None;
    }
    truncate_chars_internal(string, width, ellipsis, config.line_width_multiplier?)
}

fn truncate_chars_internal(string: &str, width: i32, ellipsis: bool, line_width_multiplier: f32) -> Option<String> {
    let reserved = if ellipsis { width - 1 } else { width };
    let max_w = (reserved as f32 * line_width_multiplier).round() as i32;
    let mut out = String::with_capacity(string.len());
    let mut acc = 0i32;
    let mut truncated = false;
    for c in string.chars() {
        let cw = c.width().unwrap_or(0) as i32;
        if cw == 0 {
            out.push(c);
            continue;
        }
        if acc + cw > max_w {
            truncated = true;
            break;
        }
        acc += cw;
        out.push(c);
    }
    if !truncated {
        return None;
    }
    if ellipsis {
        out.push('\u{2026}');
    }
    Some(out)
}