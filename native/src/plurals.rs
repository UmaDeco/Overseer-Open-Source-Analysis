//! gettext plural-forms expression parser / resolver.
//! Modified from https://github.com/justinas/gettext (MIT, (c) 2016 Justinas Stankevicius), via
//! the upstream localization framework this engine was ported from. Limitation: no operator precedence — use parentheses.
//!
//! Used by the localization template engine's `$(plural ...)` / `$(ordinal ...)` filters: a pack's
//! `config.json` gives a C-style boolean expression in `n` (e.g. `n != 1`) and `resolve(n)` returns
//! the plural-form index.

#![allow(dead_code)]

use self::Ast::*;
use self::Resolver::*;

/// Plural parse error (a malformed plural_form expression); non-fatal — callers fall back to form 0.
pub type PErr = &'static str;

#[derive(Clone, Debug)]
pub enum Resolver {
    /// A boolean expression (use `Ast::parse`).
    Expr(Ast),
    /// A plain function.
    Function(fn(u64) -> usize),
}

impl Default for Resolver {
    fn default() -> Self {
        Resolver::Function(|_| 0)
    }
}

impl Resolver {
    /// The correct plural-form index for `n` objects.
    pub fn resolve(&self, n: u64) -> usize {
        match *self {
            Expr(ref ast) => ast.resolve(n),
            Function(ref f) => f(n),
        }
    }
}

/// Finds the index of a pattern, outside of parentheses.
fn index_of(src: &str, pat: &str) -> Option<usize> {
    src.chars()
        .fold((None, 0, 0, 0), |(match_index, i, n_matches, paren_level), ch| {
            if let Some(x) = match_index {
                (Some(x), i, n_matches, paren_level)
            } else {
                let new_par_lvl = match ch {
                    '(' => paren_level + 1,
                    ')' => paren_level - 1,
                    _ => paren_level,
                };
                if Some(ch) == pat.chars().nth(n_matches) {
                    let length = n_matches + 1;
                    if length == pat.len() && new_par_lvl == 0 {
                        (Some(i - n_matches), i + 1, length, new_par_lvl)
                    } else {
                        (match_index, i + 1, length, new_par_lvl)
                    }
                } else {
                    (match_index, i + 1, 0, new_par_lvl)
                }
            }
        })
        .0
}

#[derive(Clone, Debug, PartialEq)]
pub enum Ast {
    /// `x ? a : b`
    Ternary(Box<Ast>, Box<Ast>, Box<Ast>),
    /// the `n` variable
    N,
    Integer(u64),
    Op(Operator, Box<Ast>, Box<Ast>),
    Not(Box<Ast>),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Operator {
    Equal,
    NotEqual,
    GreaterOrEqual,
    SmallerOrEqual,
    Greater,
    Smaller,
    And,
    Or,
    Plus,
    Minus,
    Divide,
    Multiply,
    Modulo,
}

impl Ast {
    fn resolve(&self, n: u64) -> usize {
        match *self {
            Ternary(ref cond, ref ok, ref nok) => {
                if cond.resolve(n) == 0 {
                    nok.resolve(n)
                } else {
                    ok.resolve(n)
                }
            }
            N => n as usize,
            Integer(x) => x as usize,
            Op(ref op, ref lhs, ref rhs) => match *op {
                Operator::Equal => (lhs.resolve(n) == rhs.resolve(n)) as usize,
                Operator::NotEqual => (lhs.resolve(n) != rhs.resolve(n)) as usize,
                Operator::GreaterOrEqual => (lhs.resolve(n) >= rhs.resolve(n)) as usize,
                Operator::SmallerOrEqual => (lhs.resolve(n) <= rhs.resolve(n)) as usize,
                Operator::Greater => (lhs.resolve(n) > rhs.resolve(n)) as usize,
                Operator::Smaller => (lhs.resolve(n) < rhs.resolve(n)) as usize,
                Operator::And => (lhs.resolve(n) != 0 && rhs.resolve(n) != 0) as usize,
                Operator::Or => (lhs.resolve(n) != 0 || rhs.resolve(n) != 0) as usize,
                Operator::Plus => lhs.resolve(n) + rhs.resolve(n),
                Operator::Minus => lhs.resolve(n) - rhs.resolve(n),
                Operator::Divide => lhs.resolve(n) / rhs.resolve(n),
                Operator::Multiply => lhs.resolve(n) * rhs.resolve(n),
                Operator::Modulo => lhs.resolve(n) % rhs.resolve(n),
            },
            Not(ref val) => match val.resolve(n) {
                0 => 1,
                _ => 0,
            },
        }
    }

    pub fn parse(src: &str) -> Result<Ast, PErr> {
        Self::parse_parens(src.trim())
    }

    fn parse_parens(src: &str) -> Result<Ast, PErr> {
        if src.starts_with('(') {
            let end = src[1..src.len() - 1]
                .chars()
                .fold((1, 2), |(level, index), ch| match (level, ch) {
                    (0, '(') => (level + 1, index + 1),
                    (0, _) => (level, index),
                    (_, '(') => (level + 1, index + 1),
                    (_, ')') => (level - 1, index + 1),
                    (_, _) => (level, index + 1),
                })
                .1;
            if end == src.len() {
                Ast::parse(src[1..src.len() - 1].trim())
            } else {
                Ast::parse_and(src.trim())
            }
        } else {
            Ast::parse_and(src.trim())
        }
    }

    fn parse_and(src: &str) -> Result<Ast, PErr> {
        if let Some(i) = index_of(src, "&&") {
            Ok(Ast::Op(Operator::And, Box::new(Ast::parse(&src[0..i])?), Box::new(Ast::parse(&src[i + 2..])?)))
        } else {
            Self::parse_or(src)
        }
    }

    fn parse_or(src: &str) -> Result<Ast, PErr> {
        if let Some(i) = index_of(src, "||") {
            Ok(Ast::Op(Operator::Or, Box::new(Ast::parse(&src[0..i])?), Box::new(Ast::parse(&src[i + 2..])?)))
        } else {
            Self::parse_ternary(src)
        }
    }

    fn parse_ternary(src: &str) -> Result<Ast, PErr> {
        if let Some(i) = index_of(src, "?") {
            if let Some(l) = index_of(src, ":") {
                Ok(Ast::Ternary(
                    Box::new(Ast::parse(&src[0..i])?),
                    Box::new(Ast::parse(&src[i + 1..l])?),
                    Box::new(Ast::parse(&src[l + 1..])?),
                ))
            } else {
                Err("plural parse error")
            }
        } else {
            Self::parse_ge(src)
        }
    }

    fn parse_ge(src: &str) -> Result<Ast, PErr> {
        if let Some(i) = index_of(src, ">=") {
            Ok(Ast::Op(Operator::GreaterOrEqual, Box::new(Ast::parse(&src[0..i])?), Box::new(Ast::parse(&src[i + 2..])?)))
        } else {
            Self::parse_gt(src)
        }
    }

    fn parse_gt(src: &str) -> Result<Ast, PErr> {
        if let Some(i) = index_of(src, ">") {
            Ok(Ast::Op(Operator::Greater, Box::new(Ast::parse(&src[0..i])?), Box::new(Ast::parse(&src[i + 1..])?)))
        } else {
            Self::parse_le(src)
        }
    }

    fn parse_le(src: &str) -> Result<Ast, PErr> {
        if let Some(i) = index_of(src, "<=") {
            Ok(Ast::Op(Operator::SmallerOrEqual, Box::new(Ast::parse(&src[0..i])?), Box::new(Ast::parse(&src[i + 2..])?)))
        } else {
            Self::parse_lt(src)
        }
    }

    fn parse_lt(src: &str) -> Result<Ast, PErr> {
        if let Some(i) = index_of(src, "<") {
            Ok(Ast::Op(Operator::Smaller, Box::new(Ast::parse(&src[0..i])?), Box::new(Ast::parse(&src[i + 1..])?)))
        } else {
            Self::parse_eq(src)
        }
    }

    fn parse_eq(src: &str) -> Result<Ast, PErr> {
        if let Some(i) = index_of(src, "==") {
            Ok(Ast::Op(Operator::Equal, Box::new(Ast::parse(&src[0..i])?), Box::new(Ast::parse(&src[i + 2..])?)))
        } else {
            Self::parse_neq(src)
        }
    }

    fn parse_neq(src: &str) -> Result<Ast, PErr> {
        if let Some(i) = index_of(src, "!=") {
            Ok(Ast::Op(Operator::NotEqual, Box::new(Ast::parse(&src[0..i])?), Box::new(Ast::parse(&src[i + 2..])?)))
        } else {
            Self::parse_plus(src)
        }
    }

    fn parse_plus(src: &str) -> Result<Ast, PErr> {
        if let Some(i) = index_of(src, "+") {
            Ok(Ast::Op(Operator::Plus, Box::new(Ast::parse(&src[0..i])?), Box::new(Ast::parse(&src[i + 1..])?)))
        } else {
            Self::parse_minus(src.trim())
        }
    }

    fn parse_minus(src: &str) -> Result<Ast, PErr> {
        if let Some(i) = index_of(src, "-") {
            Ok(Ast::Op(Operator::Minus, Box::new(Ast::parse(&src[0..i])?), Box::new(Ast::parse(&src[i + 1..])?)))
        } else {
            Self::parse_divide(src.trim())
        }
    }

    fn parse_divide(src: &str) -> Result<Ast, PErr> {
        if let Some(i) = index_of(src, "/") {
            Ok(Ast::Op(Operator::Divide, Box::new(Ast::parse(&src[0..i])?), Box::new(Ast::parse(&src[i + 1..])?)))
        } else {
            Self::parse_multiply(src.trim())
        }
    }

    fn parse_multiply(src: &str) -> Result<Ast, PErr> {
        if let Some(i) = index_of(src, "*") {
            Ok(Ast::Op(Operator::Multiply, Box::new(Ast::parse(&src[0..i])?), Box::new(Ast::parse(&src[i + 1..])?)))
        } else {
            Self::parse_mod(src.trim())
        }
    }

    fn parse_mod(src: &str) -> Result<Ast, PErr> {
        if let Some(i) = index_of(src, "%") {
            Ok(Ast::Op(Operator::Modulo, Box::new(Ast::parse(&src[0..i])?), Box::new(Ast::parse(&src[i + 1..])?)))
        } else {
            Self::parse_not(src.trim())
        }
    }

    fn parse_not(src: &str) -> Result<Ast, PErr> {
        if index_of(src, "!") == Some(0) {
            Ok(Ast::Not(Box::new(Ast::parse(&src[1..])?)))
        } else {
            Self::parse_int(src.trim())
        }
    }

    fn parse_int(src: &str) -> Result<Ast, PErr> {
        if let Ok(x) = src.parse::<u64>() {
            Ok(Ast::Integer(x))
        } else {
            Self::parse_n(src.trim())
        }
    }

    fn parse_n(src: &str) -> Result<Ast, PErr> {
        if src == "n" {
            Ok(Ast::N)
        } else {
            Err("plural parse error")
        }
    }
}

#[cfg(test)]
mod plural_tests {
    use super::{Ast, Resolver};

    // The real UmaTL config: plural_form "n == 1 ? 0 : 1" (English: 1 = singular, else plural),
    // ordinal_form "(((n+9)%10)<3 && ((n+90)%100)>10) ? ((n+9)%10) : 3" (st/nd/rd, else th).
    fn plural() -> Resolver {
        Resolver::Expr(Ast::parse("n == 1 ? 0 : 1").expect("plural parses"))
    }
    fn ordinal() -> Resolver {
        Resolver::Expr(Ast::parse("(((n+9)%10)<3 && ((n+90)%100)>10) ? ((n+9)%10) : 3").expect("ordinal parses"))
    }

    #[test]
    fn english_plural_singular_vs_plural() {
        let p = plural();
        assert_eq!(p.resolve(1), 0); // "1 turn"
        assert_eq!(p.resolve(0), 1); // "0 turns"
        assert_eq!(p.resolve(2), 1); // "2 turns"
        assert_eq!(p.resolve(21), 1);
    }

    #[test]
    fn english_ordinal_suffix_index() {
        // ordinal_types = [$st, $nd, $rd, $th]; resolver returns 0/1/2 for st/nd/rd, else 3 (th).
        let o = ordinal();
        assert_eq!(o.resolve(1), 0);  // 1st
        assert_eq!(o.resolve(2), 1);  // 2nd
        assert_eq!(o.resolve(3), 2);  // 3rd
        assert_eq!(o.resolve(4), 3);  // 4th
        assert_eq!(o.resolve(11), 3); // 11th (not 11st)
        assert_eq!(o.resolve(12), 3); // 12th
        assert_eq!(o.resolve(13), 3); // 13th
        assert_eq!(o.resolve(21), 0); // 21st
        assert_eq!(o.resolve(22), 1); // 22nd
        assert_eq!(o.resolve(113), 3); // 113th
    }
}
