//! SQLite SELECT state machine (ported from the upstream localization framework's SQL hook layer).
//!
//! The game reads localizable text out of `master.mdb` via `LibNative.Sqlite3`. We parse each SELECT
//! (with `sqlparser`) to learn its table, which projection column holds the text, and which WHERE
//! placeholders bind the lookup keys; then, on `GetText`, we return the pack's translation for that
//! row instead of the DB value. Pure Rust except `Column::try_get_int`, which reads a bound int back
//! from the live query object via `loc_db::get_int`.
//!
//! NOTE: the skill-learning-screen fit/wrap sizing (the upstream `fit_text`/`wrap_fit_text`) is deferred
//! to the wave that ports the shared wrap engine — skill names/descriptions still translate here,
//! just without that one screen's special sizing.

#![allow(dead_code)]

use sqlparser::ast;

use crate::il2cpp::{self, Object};

pub trait SelectQueryState {
    fn add_column(&mut self, idx: i32, name: &str);
    fn add_param(&mut self, idx: i32, name: &str);
    fn bind_int(&mut self, idx: i32, value: i32);
    fn get_text(&self, query: Object, idx: i32) -> Option<Object>;
}

#[derive(Default)]
struct Column {
    select_idx: Option<i32>,
    param_idx: Option<i32>,
    int_value: Option<i32>,
}

impl Column {
    fn is_select_idx(&self, idx: i32) -> bool {
        self.select_idx == Some(idx)
    }
    fn is_param_idx(&self, idx: i32) -> bool {
        self.param_idx == Some(idx)
    }
    fn try_bind_int(&mut self, idx: i32, value: i32) {
        if self.is_param_idx(idx) {
            self.int_value = Some(value);
        }
    }
    fn try_get_int(&self, query: Object) -> Option<i32> {
        self.select_idx.map(|idx| crate::loc_db::get_int(query, idx))
    }
    fn value_or_try_get_int(&self, query: Object) -> Option<i32> {
        self.int_value.or_else(|| self.try_get_int(query))
    }
}

// ── text_data (UI text + skill name cat 47 / desc cat 48) ────────────────────
#[derive(Default)]
pub struct TextDataQuery {
    text: Column,     // SELECT
    category: Column, // WHERE
    index: Column,    // WHERE
}

/// Finalize a PACK string on its way into the game: expand `$(plural …)` / `$(ordinal …)` /
/// `$(month …)` templates against the pack's rules, then mark the result as OUR OUTPUT so the
/// set_text hooks forward it untouched — without the marking, the MTL tiers would treat the pack's
/// human translation as a fresh source and machine-translate it into garble (the exact feedback
/// loop `is_own_output` exists to stop). Every dict lookup in this file must exit through here.
pub(crate) fn pack_string(s: &str) -> Object {
    let expanded = crate::template::eval(s);
    crate::loc_settext::record_emitted(&expanded);
    il2cpp::new_string(&expanded)
}

impl TextDataQuery {
    fn get_skill_name(index: i32) -> Option<Object> {
        let ld = crate::localize::data();
        if ld.config.disable_skill_name_translation {
            return None;
        }
        ld.text_data_dict.get(&47).and_then(|c| c.get(&index)).map(|s| pack_string(s))
    }

    fn get_skill_desc(mut index: i32) -> Option<Object> {
        // Inherited skills use a different id space.
        if index > 900000 && index < 1000000 {
            index -= 800000;
        }
        let ld = crate::localize::data();
        ld.text_data_dict.get(&48).and_then(|c| c.get(&index)).map(|s| pack_string(s))
    }
}

impl SelectQueryState for TextDataQuery {
    fn add_column(&mut self, idx: i32, name: &str) {
        if name == "text" {
            self.text.select_idx = Some(idx);
        }
    }
    fn add_param(&mut self, idx: i32, name: &str) {
        match name {
            "category" => self.category.param_idx = Some(idx),
            "index" => self.index.param_idx = Some(idx),
            _ => {}
        }
    }
    fn bind_int(&mut self, idx: i32, value: i32) {
        self.category.try_bind_int(idx, value);
        self.index.try_bind_int(idx, value);
    }
    fn get_text(&self, _query: Object, idx: i32) -> Option<Object> {
        if !self.text.is_select_idx(idx) {
            return None;
        }
        let category = self.category.int_value?;
        let index = self.index.int_value?;
        match category {
            47 => return Self::get_skill_name(index),
            48 => return Self::get_skill_desc(index),
            _ => {}
        }
        crate::localize::data()
            .text_data_dict
            .get(&category)
            .and_then(|c| c.get(&index))
            .map(|s| pack_string(s))
    }
}

// ── character_system_text ────────────────────────────────────────────────────
#[derive(Default)]
pub struct CharacterSystemTextQuery {
    text: Column,         // SELECT
    character_id: Column, // WHERE
    voice_id: Column,     // may appear in both
}

impl SelectQueryState for CharacterSystemTextQuery {
    fn add_column(&mut self, idx: i32, name: &str) {
        match name {
            "text" => self.text.select_idx = Some(idx),
            "voice_id" => self.voice_id.select_idx = Some(idx),
            _ => {}
        }
    }
    fn add_param(&mut self, idx: i32, name: &str) {
        match name {
            "character_id" => self.character_id.param_idx = Some(idx),
            "voice_id" => self.voice_id.param_idx = Some(idx),
            _ => {}
        }
    }
    fn bind_int(&mut self, idx: i32, value: i32) {
        self.character_id.try_bind_int(idx, value);
        self.voice_id.try_bind_int(idx, value);
    }
    fn get_text(&self, query: Object, idx: i32) -> Option<Object> {
        if !self.text.is_select_idx(idx) {
            return None;
        }
        let character_id = self.character_id.int_value?;
        let voice_id = self.voice_id.value_or_try_get_int(query)?;
        crate::localize::data()
            .character_system_text_dict
            .get(&character_id)
            .and_then(|c| c.get(&voice_id))
            .map(|s| pack_string(s))
    }
}

// ── race_jikkyo_comment ──────────────────────────────────────────────────────
#[derive(Default)]
pub struct RaceJikkyoCommentQuery {
    id: Column,
    message: Column,
}

impl SelectQueryState for RaceJikkyoCommentQuery {
    fn add_column(&mut self, idx: i32, name: &str) {
        match name {
            "id" => self.id.select_idx = Some(idx),
            "message" => self.message.select_idx = Some(idx),
            _ => {}
        }
    }
    fn add_param(&mut self, _idx: i32, _name: &str) {}
    fn bind_int(&mut self, _idx: i32, _value: i32) {}
    fn get_text(&self, query: Object, idx: i32) -> Option<Object> {
        if !self.message.is_select_idx(idx) {
            return None;
        }
        let id = self.id.try_get_int(query)?;
        crate::localize::data().race_jikkyo_comment_dict.get(&id).map(|s| pack_string(s))
    }
}

// ── race_jikkyo_message ──────────────────────────────────────────────────────
#[derive(Default)]
pub struct RaceJikkyoMessageQuery {
    id: Column,
    message: Column,
}

impl SelectQueryState for RaceJikkyoMessageQuery {
    fn add_column(&mut self, idx: i32, name: &str) {
        match name {
            "id" => self.id.select_idx = Some(idx),
            "message" => self.message.select_idx = Some(idx),
            _ => {}
        }
    }
    fn add_param(&mut self, _idx: i32, _name: &str) {}
    fn bind_int(&mut self, _idx: i32, _value: i32) {}
    fn get_text(&self, query: Object, idx: i32) -> Option<Object> {
        if !self.message.is_select_idx(idx) {
            return None;
        }
        let id = self.id.try_get_int(query)?;
        crate::localize::data().race_jikkyo_message_dict.get(&id).map(|s| pack_string(s))
    }
}

// ── sqlparser AST extensions (verbatim from the upstream port source; sqlparser is version-pinned to keep AST parity) ─────────────────────────
pub trait SelectExt {
    fn get_first_table_name(&self) -> Option<&String>;
}

impl SelectExt for ast::Select {
    fn get_first_table_name(&self) -> Option<&String> {
        if let Some(table_with_joins) = self.from.first() {
            if let ast::TableFactor::Table { name: object_name, .. } = &table_with_joins.relation {
                if let Some(ident) = object_name.0.first() {
                    return Some(&ident.value);
                }
            }
        }
        None
    }
}

pub trait SelectItemExt {
    fn get_unnamed_expr_ident(&self) -> Option<&String>;
}

impl SelectItemExt for ast::SelectItem {
    fn get_unnamed_expr_ident(&self) -> Option<&String> {
        if let ast::SelectItem::UnnamedExpr(expr) = self {
            return expr.get_ident_value();
        }
        None
    }
}

pub trait ExprExt {
    fn binary_op_iter(&self) -> BinaryOpIter<'_>;
    fn get_ident_value(&self) -> Option<&String>;
    fn is_placeholder_value(&self) -> bool;
}

impl ExprExt for ast::Expr {
    fn binary_op_iter(&self) -> BinaryOpIter<'_> {
        BinaryOpIter { stack: vec![self] }
    }
    fn get_ident_value(&self) -> Option<&String> {
        if let ast::Expr::Identifier(ident) = self {
            return Some(&ident.value);
        }
        None
    }
    fn is_placeholder_value(&self) -> bool {
        matches!(self, ast::Expr::Value(ast::Value::Placeholder(_)))
    }
}

pub struct BinaryOpIter<'a> {
    stack: Vec<&'a ast::Expr>,
}

pub struct BinaryOpRef<'a> {
    pub left: &'a ast::Expr,
    pub op: &'a ast::BinaryOperator,
    pub right: &'a ast::Expr,
}

impl<'a> Iterator for BinaryOpIter<'a> {
    type Item = BinaryOpRef<'a>;
    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let expr = self.stack.pop()?;
            let ast::Expr::BinaryOp { left, op, right } = expr else {
                continue;
            };
            self.stack.push(right);
            self.stack.push(left); // left pop'd first
            return Some(BinaryOpRef { left, op, right });
        }
    }
}
