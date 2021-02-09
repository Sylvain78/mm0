//! The MMC parser, which takes lisp literals and maps them to MMC AST.
use std::mem;
use std::convert::TryInto;
use std::collections::{HashMap, hash_map::Entry};
use crate::util::{Span, FileSpan, SliceExt};
use crate::elab::{Result, ElabError,
  environment::{AtomId, Type as EType},
  lisp::{LispKind, LispVal, Uncons, print::FormatEnv},
  local_context::try_get_span};
use super::types::{FieldName, Keyword, Mm0Expr, Mm0ExprNode, Size, Spanned, entity::{ProcTc, Intrinsic}};
use super::types::entity::{Entity, Prim, PrimType, PrimProp, PrimOp, TypeTy};
#[allow(clippy::wildcard_imports)] use super::types::parse::*;

#[derive(Debug, DeepSizeOf)]
#[allow(variant_size_differences)]
enum ItemGroup {
  Item(Item),
  Global(Uncons),
  Const(Uncons),
}

#[derive(Debug, DeepSizeOf)]
enum ItemIterInner {
  New,
  Global(Uncons),
  Const(Uncons),
}

/// An iterator over items. This is not a proper iterator in the sense of implementing `Iterator`,
/// but it can be used with the [`Parser::parse_next_item`] method to extract a stream of items.
#[derive(Debug, DeepSizeOf)]
pub struct ItemIter(ItemIterInner, Uncons);

impl ItemIter {
  /// Construct a new iterator from an `I: Iterator<Item=LispVal>`.
  #[must_use] pub fn new(e: LispVal) -> Self {
    Self(ItemIterInner::New, Uncons::New(e))
  }
}

/// The parser, which has no real state of its own but needs access to the
/// formatting environment for printing, and the keyword list.
#[derive(Copy, Clone, Debug)]
pub struct Parser<'a> {
  /// The formatting environment.
  pub fe: FormatEnv<'a>,
  /// The keyword list.
  pub kw: &'a HashMap<AtomId, Keyword>,
}

fn head_atom(e: &LispVal) -> Option<(AtomId, Uncons)> {
  let mut u = Uncons::from(e.clone());
  Some((u.next()?.as_atom()?, u))
}

/// Try to parse an atom or syntax object into a keyword.
#[must_use] pub fn as_keyword<S: std::hash::BuildHasher>(
  kw: &HashMap<AtomId, Keyword, S>, e: &LispVal
) -> Option<Keyword> {
  e.unwrapped(|e| match *e {
    LispKind::Atom(a) => kw.get(&a).copied(),
    LispKind::Syntax(s) => Keyword::from_syntax(s),
    _ => None
  })
}

/// Try to parse the head keyword of an expression `(KEYWORD args..)`,
/// and return the pair `(KEYWORD, args)` on success.
#[must_use] pub fn head_keyword<S: std::hash::BuildHasher>(
  kw: &HashMap<AtomId, Keyword, S>, e: &LispVal
) -> Option<(Keyword, Uncons)> {
  let mut u = Uncons::from(e.clone());
  Some((as_keyword(kw, &u.next()?)?, u))
}

fn try_get_fspan(fsp: &FileSpan, e: &LispVal) -> FileSpan {
  FileSpan { file: fsp.file.clone(), span: try_get_span(fsp, e) }
}

/// Uses the span of the [`LispVal`] to construct a [`Spanned`]`<T>`.
pub fn spanned<T>(fsp: &FileSpan, e: &LispVal, k: T) -> Spanned<T> {
  Spanned {span: try_get_fspan(fsp, e), k}
}

impl<'a> Parser<'a> {

  fn head_keyword(&self, e: &LispVal) -> Option<(Keyword, Uncons)> { head_keyword(self.kw, e) }

  fn parse_variant(&self, base: &FileSpan, e: &LispVal) -> Option<Box<Variant>> {
    if let Some((Keyword::Variant, mut u)) = self.head_keyword(e) {
      Some(Box::new(spanned(base, e, (u.next()?, match u.next() {
        None => VariantType::Down,
        Some(e) => match self.kw.get(&e.as_atom()?) {
          Some(Keyword::Lt) => VariantType::UpLt(u.next()?),
          Some(Keyword::Le) => VariantType::UpLe(u.next()?),
          _ => return None
        }
      }))))
    } else {None}
  }

  fn parse_arg1(&self, base: &FileSpan, e: LispVal, name_required: bool) -> Result<TuplePattern> {
    if let Some((AtomId::COLON, _)) = head_atom(&e) {
      Ok(self.parse_tuple_pattern(base, false, e)?)
    } else if name_required {
      let a = e.as_atom().ok_or_else(||
        ElabError::new_e(try_get_span(base, &e), "argument syntax error: expecting identifier"))?;
      Ok(spanned(base, &e, TuplePatternKind::Name(a == AtomId::UNDER, a)))
    } else {
      let span = try_get_fspan(base, &e);
      let under = Box::new(Spanned {span: span.clone(), k: TuplePatternKind::UNDER});
      Ok(Spanned {span, k: TuplePatternKind::Typed(under, e)})
    }
  }

  fn parse_arg(&self, base: &FileSpan, e: LispVal, name_required: bool,
    mut push_arg: impl FnMut(Span, MutOut, TuplePattern) -> Result<()>,
  ) -> Result<()> {
    match self.head_keyword(&e) {
      Some((Keyword::Mut, u)) => for e in u {
        push_arg(try_get_span(base, &e), MutOut::Mut, self.parse_arg1(base, e, name_required)?)?
      }
      Some((Keyword::Out, mut u)) => {
        let (a, e) = match (u.next(), u.next(), u.is_empty()) {
          (Some(e), None, _) => (AtomId::UNDER, e),
          (Some(e1), Some(e), true) => {
            let a = e1.as_atom().ok_or_else(||
              ElabError::new_e(try_get_span(base, &e1), "'out' syntax error"))?;
            (a, e)
          }
          _ => return Err(ElabError::new_e(try_get_span(base, &e), "'out' syntax error")),
        };
        push_arg(try_get_span(base, &e), MutOut::Out(a), self.parse_arg1(base, e, name_required)?)?
      }
      _ => push_arg(try_get_span(base, &e), MutOut::None, self.parse_arg1(base, e, name_required)?)?
    }
    Ok(())
  }

  /// Parse a tuple pattern.
  pub fn parse_tuple_pattern(&self, base: &FileSpan, ghost: bool, e: LispVal) -> Result<TuplePattern> {
    Ok(Spanned {span: try_get_fspan(base, &e), k: {
      if let Some(a) = e.as_atom() {
        TuplePatternKind::Name(ghost || a == AtomId::UNDER, a)
      } else if e.is_list() {
        match self.head_keyword(&e) {
          Some((Keyword::Colon, mut u)) => {
            if let (Some(e), Some(ty), true) = (u.next(), u.next(), u.is_empty()) {
              TuplePatternKind::Typed(Box::new(self.parse_tuple_pattern(base, ghost, e)?), ty)
            } else {
              return Err(ElabError::new_e(try_get_span(base, &e), "':' syntax error"))
            }
          }
          Some((Keyword::Ghost, u)) => {
            let mut args = vec![];
            for e in u {args.push(self.parse_tuple_pattern(base, true, e)?)}
            if args.len() == 1 {
              return Ok(args.drain(..).next().expect("nonempty"))
            }
            TuplePatternKind::Tuple(args)
          }
          _ => {
            let mut args = vec![];
            for e in Uncons::from(e) {args.push(self.parse_tuple_pattern(base, ghost, e)?)}
            TuplePatternKind::Tuple(args)
          }
        }
      } else {
        return Err(ElabError::new_e(try_get_span(base, &e),
          format!("tuple pattern syntax error: {}", self.fe.to(&e))))
      }
    }})
  }

  fn parse_decl_asgn(&self, base: &FileSpan, e: &LispVal) -> Result<ExprKind> {
    match self.head_keyword(e) {
      Some((Keyword::ColonEq, mut u)) =>
        if let (Some(lhs), Some(rhs), true) = (u.next(), u.next(), u.is_empty()) {
          let lhs = Box::new(self.parse_tuple_pattern(base, false, lhs)?);
          return Ok(ExprKind::Let {lhs, rhs, with: Renames::default()})
        },
      Some((Keyword::ArrowL, mut u)) =>
        if let (Some(lhs), Some(rhs), true) = (u.next(), u.next(), u.is_empty()) {
          return Ok(ExprKind::Assign {lhs, rhs, with: Renames::default()})
        },
      _ => {}
    }
    Err(ElabError::new_e(try_get_span(base, e), "decl: syntax error"))
  }

  fn parse_decl(&self, base: &FileSpan, e: &LispVal) -> Result<Spanned<(Box<TuplePattern>, LispVal)>> {
    if let ExprKind::Let {lhs, rhs, ..} = self.parse_decl_asgn(base, e)? {
      Ok(spanned(base, e, (lhs, rhs)))
    } else {
      Err(ElabError::new_e(try_get_span(base, e), "decl: assignment not allowed here"))
    }
  }

  fn parse_rename(&self, base: &FileSpan, e: &LispVal, with: &mut Renames) -> Result<bool> {
    match self.head_keyword(e) {
      Some((Keyword::ArrowL, mut u)) => if let (Some(to), Some(from), true) =
        (u.next().and_then(|e| e.as_atom()), u.next().and_then(|e| e.as_atom()), u.is_empty()) {
        with.new.push((from, to));
        return Ok(true)
      },
      Some((Keyword::ArrowR, mut u)) => if let (Some(from), Some(to), true) =
        (u.next().and_then(|e| e.as_atom()), u.next().and_then(|e| e.as_atom()), u.is_empty()) {
        with.old.push((from, to));
        return Ok(true)
      },
      _ => if let Some(a) = e.as_atom() {
        with.new.push((a, a));
        return Ok(true)
      } else { return Ok(false) }
    }
    Err(ElabError::new_e(try_get_span(base, e), "with: expected {old -> old'} or {new' <- new}"))
  }

  /// Parse an MMC expression (shallowly), returning a [`parser::Expr`](Expr)
  /// containing [`LispVal`]s for subexpressions.
  pub fn parse_expr(&self, base: &FileSpan, e: LispVal) -> Result<Expr> {
    Ok(Spanned {span: try_get_fspan(base, &e), k: match &*e.unwrapped_arc() {
      &LispKind::Atom(AtomId::UNDER) => ExprKind::Hole,
      &LispKind::Atom(a) => ExprKind::Var(a),
      &LispKind::Bool(b) => ExprKind::Bool(b),
      LispKind::Number(n) => ExprKind::Int(n.clone()),
      LispKind::DottedList(es, r) if !r.is_list() && es.len() == 1 => if let Some(a) = r.as_atom() {
        ExprKind::Proj(es[0].clone(), FieldName::Named(a))
      } else {
        match r.as_int(|n| n.try_into()) {
          Some(Ok(i)) => ExprKind::Proj(es[0].clone(), FieldName::Number(i)),
          Some(Err(_)) => return Err(ElabError::new_e(try_get_span(base, &e), "field access: index out of range")),
          None => return Err(ElabError::new_e(try_get_span(base, &e), "field access syntax error")),
        }
      },
      LispKind::List(_) | LispKind::DottedList(_, _) => match self.head_keyword(&e) {
        Some((Keyword::ColonEq, _)) |
        Some((Keyword::ArrowL, _)) => self.parse_decl_asgn(base, &e)?,
        Some((Keyword::With, mut u)) => {
          let mut ret = self.parse_decl_asgn(base, &u.next().ok_or_else(||
            ElabError::new_e(try_get_span(base, &e), "'with' syntax error"))?)?;
          let with = match &mut ret {
            ExprKind::Let {with, ..} | ExprKind::Assign {with, ..} => with,
            _ => unreachable!()
          };
          for e in u {
            if !self.parse_rename(base, &e, with)? {
              for e in Uncons::New(e) {
                if !self.parse_rename(base, &e, with)? {
                  return Err(ElabError::new_e(try_get_span(base, &e),
                    "with: expected {old -> old'} or {new' <- new}"))
                }
              }
            }
          }
          ret
        }
        Some((Keyword::If, mut u)) => {
          let err = || ElabError::new_e(try_get_span(base, &e), "if: syntax error");
          let mut branches = vec![];
          let mut push = |(cond, tru)| {
            let (hyp, cond) = match self.head_keyword(&cond) {
              Some((Keyword::Colon, mut u)) =>
                if let (Some(h), Some(cond), true) = (u.next(), u.next(), u.is_empty()) {
                  let h = h.as_atom().ok_or_else(||
                    ElabError::new_e(try_get_span(base, &h), "expecting hypothesis name"))?;
                  (if h == AtomId::UNDER {None} else {Some(h)}, cond)
                } else {
                  return Err(ElabError::new_e(try_get_span(base, &cond), "':' syntax error"))
                },
              _ => (None, cond),
            };
            branches.push((hyp, cond, tru));
            Ok(())
          };
          let mut cond_tru = (u.next().ok_or_else(err)?, u.next().ok_or_else(err)?);
          let mut els;
          loop {
            els = u.next();
            if let Some(Keyword::Else) = els.as_ref().and_then(|e| self.kw.get(&e.as_atom()?)) {
              els = u.next()
            }
            if let Some(Keyword::If) = els.as_ref().and_then(|e| self.kw.get(&e.as_atom()?)) {
              push(mem::replace(&mut cond_tru,
                (u.next().ok_or_else(err)?, u.next().ok_or_else(err)?)))?;
            } else {break}
          }
          push(cond_tru)?;
          ExprKind::If {branches, els}
        }
        Some((Keyword::Match, mut u)) => {
          let c =  u.next().ok_or_else(||
            ElabError::new_e(try_get_span(base, &e), "match: syntax error"))?;
          let mut branches = vec![];
          for e in u {
            if let Some((Keyword::Arrow, mut u)) = self.head_keyword(&e) {
              if let (Some(p), Some(e), true) = (u.next(), u.next(), u.is_empty()) {
                branches.push((p, e));
              } else {
                return Err(ElabError::new_e(try_get_span(base, &e), "match: syntax error"))
              }
            } else {
              return Err(ElabError::new_e(try_get_span(base, &e), "match: syntax error"))
            }
          }
          ExprKind::Match(c, branches)
        }
        Some((Keyword::While, mut u)) => {
          let c = u.next().ok_or_else(||
            ElabError::new_e(try_get_span(base, &e), "while: syntax error"))?;
          let (hyp, cond) = if let Some((Keyword::Colon, mut u)) = self.head_keyword(&c) {
            let h = u.next().and_then(|e| Some(spanned(base, &e, e.as_atom()?)));
            if let (Some(h), Some(c), true) = (h, u.next(), u.is_empty()) {
              (Some(h), c)
            } else {
              return Err(ElabError::new_e(try_get_span(base, &e), "while: bad pattern"))
            }
          } else {(None, c)};
          let mut var = None;
          while let Some(e) = u.head() {
            if let Some(v) = self.parse_variant(base, &e) {
              if mem::replace(&mut var, Some(v)).is_some() {
                return Err(ElabError::new_e(try_get_span(base, &e), "while: two variants"))
              }
            } else {break}
            u.next();
          }
          ExprKind::While { hyp, cond, var, body: u }
        }
        Some((Keyword::Begin, u)) => ExprKind::Block(u),
        Some((Keyword::Entail, u)) => {
          let mut args = u.collect::<Vec<_>>();
          let last = args.pop().ok_or_else(||
            ElabError::new_e(try_get_span(base, &e), "entail: expected proof"))?;
          ExprKind::Entail(last, args)
        }
        _ => {
          let mut u = Uncons::from(e);
          match u.next() {
            None => ExprKind::Unit,
            Some(e) => if let Some((Keyword::Begin, mut u1)) = self.head_keyword(&e) {
              let name = u1.next().and_then(|e| e.as_atom()).ok_or_else(||
                ElabError::new_e(try_get_span(base, &e), "label: expected label name"))?;
              let mut args = vec![];
              let mut variant = None;
              for e in u1 {
                if let Some(v) = self.parse_variant(base, &e) {
                  if mem::replace(&mut variant, Some(v)).is_some() {
                    return Err(ElabError::new_e(try_get_span(base, &e), "label: two variants"))
                  }
                } else {
                  args.push(self.parse_arg1(base, e, true)?)
                }
              }
              ExprKind::Label(Label { name, args, variant, body: u })
            } else {
              let fsp = try_get_fspan(base, &e);
              let f = e.as_atom().ok_or_else(|| ElabError::new_e(fsp.span,
                "only variables can be called like functions"))?;
              let mut args = vec![];
              let mut variant = None;
              for e in u {
                if let Some((Keyword::Variant, u)) = self.head_keyword(&e) {
                  if mem::replace(&mut variant, Some(u.as_lisp())).is_some() {
                    return Err(ElabError::new_e(try_get_span(base, &e), "call: two variants"))
                  }
                } else {
                  args.push(e)
                }
              }
              ExprKind::Call(CallExpr {f: Spanned {span: fsp, k: f}, args, variant})
            }
          }
        }
      },
      _ => return Err(ElabError::new_e(try_get_span(base, &e), "unknown expression"))
    }})
  }

  fn parse_proc(&self, base: &FileSpan, kind: &dyn Fn(AtomId) -> Result<ProcKind>, mut u: Uncons) -> Result<Proc> {
    let e = match u.next() {
      None => return Err(ElabError::new_e(try_get_span(base, &LispVal::from(u)), "func/proc: syntax error")),
      Some(e) => e,
    };
    let mut args = vec![];
    let mut rets = vec![];
    let name = match &*e.unwrapped_arc() {
      &LispKind::Atom(a) => spanned(base, &e, a),
      LispKind::List(_) | LispKind::DottedList(_, _) => {
        let mut u = Uncons::from(e.clone());
        let e = u.next().ok_or_else(||
          ElabError::new_e(try_get_span(base, &e), "func/proc syntax error: expecting function name"))?;
        let name = e.as_atom().ok_or_else(||
          ElabError::new_e(try_get_span(base, &e), "func/proc syntax error: expecting an atom"))?;
        while let Some(e) = u.next() {
          if let Some(AtomId::COLON) = e.as_atom() { break }
          self.parse_arg(base, e, true, |_, mo, arg| {args.push((mo, arg)); Ok(())})?
        }
        for e in u {
          self.parse_arg(base, e, false, |_, mo, arg| {rets.push((mo, arg)); Ok(())})?
        }
        spanned(base, &e, name)
      }
      _ => return Err(ElabError::new_e(try_get_span(base, &e), "func/proc: syntax error"))
    };
    let kind = kind(name.k)?;
    let variant = if let Some(e) = u.head() {
      if let ProcKind::Intrinsic(_) = kind {
        return Err(ElabError::new_e(try_get_span(base, &e), "intrinsic: unexpected body"))
      }
      self.parse_variant(base, &e)
    } else {None};
    Ok(Proc {name, args, rets, variant, kind, body: u})
  }

  fn parse_name_and_tyargs(&self, base: &FileSpan, e: &LispVal) -> Result<(Spanned<AtomId>, Vec<TuplePattern>)> {
    let mut args = vec![];
    let name = match &*e.unwrapped_arc() {
      &LispKind::Atom(a) => spanned(base, e, a),
      LispKind::List(_) | LispKind::DottedList(_, _) => {
        let mut u = Uncons::from(e.clone());
        let e = u.next().ok_or_else(|| ElabError::new_e(try_get_span(base, e), "typedef: syntax error"))?;
        let a = spanned(base, &e, e.as_atom().ok_or_else(||
          ElabError::new_e(try_get_span(base, &e), "typedef: expected an atom"))?);
        for e in u { args.push(self.parse_arg1(base, e, true)?) }
        a
      },
      _ => return Err(ElabError::new_e(try_get_span(base, e), "typedef: syntax error"))
    };
    Ok((name, args))
  }

  /// Parses the input lisp literal `e` into a list of top level items and appends them to `ast`.
  fn parse_item_group(&self, base: &FileSpan, e: &LispVal) -> Result<ItemGroup> {
    Ok(match self.head_keyword(e) {
      Some((Keyword::Proc, u)) =>
        ItemGroup::Item(spanned(base, e, ItemKind::Proc(self.parse_proc(base, &|_| Ok(ProcKind::Proc), u)?))),
      Some((Keyword::Func, u)) =>
        ItemGroup::Item(spanned(base, e, ItemKind::Proc(self.parse_proc(base, &|_| Ok(ProcKind::Func), u)?))),
      Some((Keyword::Intrinsic, u)) => {
        let f = |a| Ok(ProcKind::Intrinsic(Intrinsic::from_bytes(&self.fe.env.data[a].name)
          .ok_or_else(|| ElabError::new_e(try_get_span(base, e), "unknown intrinsic"))?));
        ItemGroup::Item(spanned(base, e, ItemKind::Proc(self.parse_proc(base, &f, u)?)))
      }
      Some((Keyword::Global, u)) => ItemGroup::Global(u),
      Some((Keyword::Const, u)) => ItemGroup::Const(u),
      Some((Keyword::Typedef, mut u)) =>
        if let (Some(e1), Some(val), true) = (u.next(), u.next(), u.is_empty()) {
          let (name, args) = self.parse_name_and_tyargs(base, &e1)?;
          ItemGroup::Item(spanned(base, e, ItemKind::Typedef {name, args, val}))
        } else {
          return Err(ElabError::new_e(try_get_span(base, e), "typedef: syntax error"))
        },
      Some((Keyword::Struct, mut u)) => {
        let e1 = u.next().ok_or_else(||
          ElabError::new_e(try_get_span(base, e), "struct: expecting name"))?;
        let (name, args) = self.parse_name_and_tyargs(base, &e1)?;
        let mut fields = vec![];
        for e in u { fields.push(self.parse_arg1(base, e, false)?) }
        ItemGroup::Item(spanned(base, e, ItemKind::Struct {name, args, fields}))
      }
      _ => return Err(ElabError::new_e(try_get_span(base, e),
        format!("MMC: unknown top level item: {}", self.fe.to(e))))
    })
  }

  /// Extract the next item from the provided item iterator.
  pub fn parse_next_item(&self, base: &FileSpan, ItemIter(group, u): &mut ItemIter) -> Result<Option<Item>> {
    Ok(loop {
      match group {
        ItemIterInner::New => if let Some(e) = u.next() {
          match self.parse_item_group(base, &e)? {
            ItemGroup::Item(it) => break Some(it),
            ItemGroup::Global(u2) => *group = ItemIterInner::Global(u2),
            ItemGroup::Const(u2) => *group = ItemIterInner::Const(u2),
          }
        } else {
          break None
        },
        ItemIterInner::Global(u2) => if let Some(e) = u2.next() {
          let Spanned {span, k: (lhs, rhs)} = self.parse_decl(base, &e)?;
          break Some(Spanned {span, k: ItemKind::Global(lhs, rhs)})
        } else {
          *group = ItemIterInner::New
        },
        ItemIterInner::Const(u2) => if let Some(e) = u2.next() {
          let Spanned {span, k: (lhs, rhs)} = self.parse_decl(base, &e)?;
          break Some(Spanned {span, k: ItemKind::Const(lhs, rhs)})
        } else {
          *group = ItemIterInner::New
        }
      }
    })
  }

  /// Parse a pattern expression (shallowly).
  pub fn parse_pattern(&self, base: &FileSpan, names: &HashMap<AtomId, Entity>, e: &LispVal) -> Result<Pattern> {
    Ok(spanned(base, e, match &*e.unwrapped_arc() {
      &LispKind::Atom(a) if matches!(names.get(&a), Some(Entity::Const(_))) => PatternKind::Const(a),
      &LispKind::Atom(a) => PatternKind::Var(a),
      LispKind::List(_) | LispKind::DottedList(_, _) => match self.head_keyword(e) {
        Some((Keyword::Colon, mut u)) =>
          if let (Some(h), Some(p), true) = (u.next(), u.next(), u.is_empty()) {
            let h = h.as_atom().ok_or_else(||
              ElabError::new_e(try_get_span(base, &h), "expecting hypothesis name"))?;
            PatternKind::Hyped(h, p)
          } else {
            return Err(ElabError::new_e(try_get_span(base, e), "':' syntax error"))
          },
        Some((Keyword::Or, u)) => PatternKind::Or(u),
        Some((Keyword::With, mut u)) =>
          if let (Some(p), Some(g), true) = (u.next(), u.next(), u.is_empty()) {
            PatternKind::With(p, g)
          } else {
            return Err(ElabError::new_e(try_get_span(base, e), "'with' syntax error"))
          },
        _ => return Err(ElabError::new_e(try_get_span(base, e), "pattern syntax error"))
      }
      LispKind::Number(n) => PatternKind::Number(n.clone()),
      _ => return Err(ElabError::new_e(try_get_span(base, e), "pattern syntax error"))
    }))
  }

  /// Parse a type (shallowly).
  pub fn parse_ty(&self, base: &FileSpan, names: &HashMap<AtomId, Entity>, e: &LispVal) -> Result<Type> {
    let mut u = Uncons::New(e.clone());
    let (head, args) = match u.next() {
      None if u.is_empty() => return Ok(spanned(base, e, TypeKind::Unit)),
      None => (u.into(), vec![]),
      Some(head) => (head, u.collect()),
    };

    Ok(spanned(base, e, {
      if let Some(name) = head.as_atom() {
        if name == AtomId::UNDER {
          return Err(ElabError::new_e(try_get_span(base, &head), "expecting a type"));
        }
        match names.get(&name) {
          Some(&Entity::Prim(Prim {ty: Some(prim), ..})) => match (prim, &*args) {
            (PrimType::Array, [ty, n]) => TypeKind::Array(ty.clone(), n.clone()),
            (PrimType::Bool, []) => TypeKind::Bool,
            (PrimType::I8, []) => TypeKind::Int(Size::S8),
            (PrimType::I16, []) => TypeKind::Int(Size::S16),
            (PrimType::I32, []) => TypeKind::Int(Size::S32),
            (PrimType::I64, []) => TypeKind::Int(Size::S64),
            (PrimType::Int, []) => TypeKind::Int(Size::Inf),
            (PrimType::U8, []) => TypeKind::UInt(Size::S8),
            (PrimType::U16, []) => TypeKind::UInt(Size::S16),
            (PrimType::U32, []) => TypeKind::UInt(Size::S32),
            (PrimType::U64, []) => TypeKind::UInt(Size::S64),
            (PrimType::Nat, []) => TypeKind::UInt(Size::Inf),
            (PrimType::Input, []) => TypeKind::Input,
            (PrimType::Output, []) => TypeKind::Output,
            (PrimType::Own, [ty]) => TypeKind::Own(ty.clone()),
            (PrimType::Ref, [ty]) => TypeKind::Shr(None, ty.clone()),
            (PrimType::RefSn, [e]) => TypeKind::RefSn(e.clone()),
            (PrimType::Sn, [e]) => TypeKind::Sn(e.clone()),
            (PrimType::List, _) => TypeKind::List(args),
            (PrimType::Struct, _) => TypeKind::Struct(
              args.into_iter().map(|e| self.parse_tuple_pattern(base, false, e)).collect::<Result<_>>()?),
            (PrimType::And, _) => TypeKind::And(args),
            (PrimType::Or, _) => TypeKind::Or(args),
            (PrimType::Moved, [ty]) => TypeKind::Moved(ty.clone()),
            (PrimType::Ghost, [ty]) => TypeKind::Ghost(ty.clone()),
            (PrimType::Uninit, [ty]) => TypeKind::Uninit(ty.clone()),
            _ => return Err(ElabError::new_e(try_get_span(base, e), "unexpected number of arguments"))
          },
          Some(&Entity::Prim(p)) if p.prop.is_some() || p.op.is_some() =>
            TypeKind::Prop(self.parse_prop(base, names, e)?.k),
          Some(Entity::Type(ty)) => if let Some(&TypeTy {tyargs, args: ref tgt}) = ty.k.ty() {
            let n = tyargs as usize;
            if args.len() != n + tgt.len() {
              return Err(ElabError::new_e(try_get_span(base, &head), "unexpected number of arguments"))
            }
            TypeKind::User(name, args[..n].cloned_box(), args[n..].cloned_box())
          } else {
            TypeKind::Error
          },
          Some(_) => return Err(ElabError::new_e(try_get_span(base, &head), "expected a type")),
          None if args.is_empty() => TypeKind::Var(name),
          None => return Err(ElabError::new_e(try_get_span(base, &head),
            format!("unknown type constructor '{}'", self.fe.to(&name)))),
        }
      } else {
        match as_keyword(self.kw, &head) {
          Some(Keyword::If) => TypeKind::If(args.try_into().map_err(|_|
            ElabError::new_e(try_get_span(base, &head), "unexpected number of arguments"))?),
          _ => return Err(ElabError::new_e(try_get_span(base, &head), "expected an atom"))
        }
      }
    }))
  }

  /// Parse a proposition (shallowly).
  pub fn parse_prop(&self, base: &FileSpan, names: &HashMap<AtomId, Entity>, e: &LispVal) -> Result<Prop> {
    let mut u = Uncons::New(e.clone());
    let (head, mut args) = match u.next() {
      None if u.is_empty() =>
        return Err(ElabError::new_e(try_get_span(base, e), "expecting a proposition, got ()")),
      None => (u.into(), vec![]),
      Some(head) => (head, u.collect()),
    };
    Ok(spanned(base, e, match head.as_atom().and_then(|name| names.get(&name)) {
      Some(Entity::Prim(Prim {prop: Some(prim), ..})) => match (prim, &*args) {
        (PrimProp::All, args1) if !args1.is_empty() => {
          let last = args.pop().expect("nonempty");
          let args = args.into_iter().map(|e| self.parse_tuple_pattern(base, false, e)).collect::<Result<_>>()?;
          PropKind::All(args, last)
        }
        (PrimProp::Ex, args1) if !args1.is_empty() => {
          let last = args.pop().expect("nonempty");
          let args = args.into_iter().map(|e| self.parse_tuple_pattern(base, false, e)).collect::<Result<_>>()?;
          PropKind::Ex(args, last)
        }
        (PrimProp::And, _) => PropKind::And(args),
        (PrimProp::Or, _) => PropKind::Or(args),
        (PrimProp::Imp, [e1, e2]) => PropKind::Imp(e1.clone(), e2.clone()),
        (PrimProp::Star, _) => PropKind::Sep(args),
        (PrimProp::Wand, [e1, e2]) => PropKind::Wand(e1.clone(), e2.clone()),
        (PrimProp::Pure, [e]) => PropKind::Mm0(e.clone()),
        (PrimProp::Moved, [ty]) => PropKind::Moved(ty.clone()),
        (PrimProp::Eq, [e1, e2]) => PropKind::Eq(e1.clone(), e2.clone()),
        _ => return Err(ElabError::new_e(try_get_span(base, &head), "incorrect number of arguments")),
      }
      _ => PropKind::Pure(self.parse_expr(base, e.clone())?.k)
    }))
  }

  /// Parse an expression that looks like a function call (shallowly).
  pub fn parse_call(&self,
    base: &FileSpan,
    names: &HashMap<AtomId, Entity>,
    mut is_label: impl FnMut(AtomId) -> bool,
    CallExpr {f: Spanned {span: fsp, k: f}, args, variant}: CallExpr
  ) -> Result<CallKind> {
    macro_rules! err {($($e:expr),*) => {
      return Err(ElabError::new_e(base, format!($($e),*)))
    }}
    if is_label(f) {
      return Ok(CallKind::Jump(f, args))
    }
    Ok(match names.get(&f) {
      None => err!("unknown function '{}'", self.fe.to(&f)),
      Some(Entity::Const(_)) => CallKind::Const(f),
      Some(Entity::Global(_)) => CallKind::Global(f),
      Some(Entity::Prim(Prim {op: Some(prim), ..})) => match (prim, &*args) {
        (PrimOp::Add, _) => CallKind::NAry(NAryCall::Add, args),
        (PrimOp::And, _) => CallKind::NAry(NAryCall::And, args),
        (PrimOp::Or, _) => CallKind::NAry(NAryCall::Or, args),
        (PrimOp::Assert, _) => CallKind::NAry(NAryCall::Assert, args),
        (PrimOp::BitAnd, _) => CallKind::NAry(NAryCall::BitAnd, args),
        (PrimOp::BitNot, _) => CallKind::NAry(NAryCall::BitNot, args),
        (PrimOp::BitOr, _) => CallKind::NAry(NAryCall::BitOr, args),
        (PrimOp::BitXor, _) => CallKind::NAry(NAryCall::BitXor, args),
        (PrimOp::Index, args) => match args {
          [arr, idx] => CallKind::Index(arr.clone(), idx.clone(), None),
          [arr, idx, pf] => CallKind::Index(arr.clone(), idx.clone(), Some(pf.clone())),
          _ => err!("expected 2 or 3 arguments"),
        },
        (PrimOp::List, _) => CallKind::NAry(NAryCall::List, args),
        (PrimOp::Mul, _) => CallKind::NAry(NAryCall::Mul, args),
        (PrimOp::Not, _) => CallKind::NAry(NAryCall::Not, args),
        (PrimOp::Le, _) => CallKind::NAry(NAryCall::Le, args),
        (PrimOp::Lt, _) => CallKind::NAry(NAryCall::Lt, args),
        (PrimOp::Eq, _) => CallKind::NAry(NAryCall::Eq, args),
        (PrimOp::Ne, _) => CallKind::NAry(NAryCall::Ne, args),
        (PrimOp::Ghost, _) => CallKind::NAry(NAryCall::GhostList, args),
        (PrimOp::Return, _) => CallKind::NAry(NAryCall::Return, args),
        (PrimOp::Sub, _) => {
          let mut it = args.into_iter();
          let first = it.next().ok_or_else(|| ElabError::new_e(base, "expected 1 or more arguments"))?;
          if it.len() == 0 { CallKind::Neg(first) }
          else { CallKind::Sub(first, it) }
        }
        (PrimOp::Shl, [a, b]) => CallKind::Shl(a.clone(), b.clone()),
        (PrimOp::Shr, [a, b]) => CallKind::Shr(a.clone(), b.clone()),
        (PrimOp::Typed, [e, ty]) => CallKind::Typed(e.clone(), ty.clone()),
        (PrimOp::As, [e, ty]) => CallKind::As(e.clone(), ty.clone()),
        (PrimOp::Shl, _) | (PrimOp::Shr, _) |
        (PrimOp::Typed, _) | (PrimOp::As, _) => err!("expected 2 arguments"),
        (PrimOp::Cast, args) => match args {
          [e] => CallKind::Cast(e.clone(), None),
          [e, pf] => CallKind::Cast(e.clone(), Some(pf.clone())),
          _ => err!("expected 1 or 2 arguments"),
        },
        (PrimOp::Pun, args) => match args {
          [e] => CallKind::Pun(e.clone(), None),
          [e, pf] => CallKind::Pun(e.clone(), Some(pf.clone())),
          _ => err!("expected 1 or 2 arguments"),
        },
        (PrimOp::Sn, args) => match args {
          [e] => CallKind::Sn(e.clone(), None),
          [e, pf] => CallKind::Sn(e.clone(), Some(pf.clone())),
          _ => err!("expected 1 or 2 arguments"),
        },
        (PrimOp::Slice, args) => match args {
          [arr, idx] => CallKind::Slice(arr.clone(), idx.clone(), None),
          [arr, idx, pf] => CallKind::Slice(arr.clone(), idx.clone(), Some(pf.clone())),
          _ => err!("expected 2 or 3 arguments"),
        },
        (PrimOp::Uninit, []) => CallKind::Uninit,
        (PrimOp::Uninit, _) => err!("expected 0 arguments"),
        (PrimOp::Pure, [e]) => CallKind::Mm0(e.clone()),
        (PrimOp::Ref, [e]) => CallKind::Ref(e.clone()),
        (PrimOp::TypeofBang, [e]) => CallKind::TypeofBang(e.clone()),
        (PrimOp::Typeof, [e]) => CallKind::Typeof(e.clone()),
        (PrimOp::Sizeof, [ty]) => CallKind::Sizeof(ty.clone()),
        (PrimOp::Pure, _) | (PrimOp::Ref, _) | (PrimOp::TypeofBang, _) |
        (PrimOp::Typeof, _) |  (PrimOp::Sizeof, _) => err!("expected 1 argument"),
        (PrimOp::Unreachable, args) => match args {
          [] => CallKind::Unreachable(None),
          [e] => CallKind::Unreachable(Some(e.clone())),
          _ => err!("expected 1 argument"),
        },
        (PrimOp::Break, args1) => if let Some(a) = args1.first().and_then(|e| e.as_atom()).filter(|&a| is_label(a)) {
          CallKind::Break(Some(a), {let mut it = args.into_iter(); it.next(); it})
        } else {
          CallKind::Break(None, args.into_iter())
        },
      }
      Some(Entity::Proc(Spanned {k: ProcTc::Typed(proc), ..})) =>
        CallKind::Call(CallExpr {f: Spanned {span: fsp, k: f}, args, variant}, proc.tyargs),
      Some(Entity::Proc(Spanned {k: ProcTc::Unchecked, ..})) => CallKind::Error,
      Some(_) => err!("parse_expr unimplemented entity type"),
    })
  }

  /// Parse an MM0 expression. This is a sort of hybrid of MMC and MM0 syntax because it is MM0 syntax
  /// in the term constructors with variables drawn from the MMC context. For example,
  /// `(begin {x := 1} {y := 2} (pure $ x + x = y $))` will work, where `+` and `=` are the MM0 term constructors
  /// `add` and `eq`, while `x` and `y` are program variables in the MMC context. (TODO: MMC antiquotation?)
  pub fn parse_mm0_expr<T>(&self, base: &FileSpan, e: LispVal,
    get: impl FnMut(&LispVal, AtomId) -> Option<T>
  ) -> Result<Mm0Expr<T>> {
    struct Mm0<'a, T, F> {
      subst: Vec<T>,
      base: &'a FileSpan,
      get: F,
      vars: HashMap<AtomId, u32>,
      dummies: Vec<AtomId>,
      p: &'a Parser<'a>
    }
    impl<T, F: FnMut(&LispVal, AtomId) -> Option<T>> Mm0<'_, T, F> {
      fn list_opt(&mut self, e: &LispVal, head: AtomId, args: Option<Uncons>) -> Result<Option<Mm0ExprNode>> {
        let tid = self.p.fe.env.term(head).ok_or_else(|| ElabError::new_e(try_get_span(self.base, e),
          format!("term '{}' not declared", self.p.fe.to(&head))))?;
        let term = &self.p.fe.env.terms[tid];
        if args.as_ref().map_or(0, Uncons::len) != term.args.len() {
          return Err(ElabError::new_e(try_get_span(self.base, e),
            format!("expected {} arguments", term.args.len())));
        }
        Ok(if let Some(u) = args {
          let mut cnst = true;
          let mut vec = Vec::with_capacity(u.len());
          let len = self.dummies.len();
          for (e, (_, arg)) in u.zip(&*term.args) {
            match *arg {
              EType::Bound(_) => {
                let a = e.as_atom().ok_or_else(||
                  ElabError::new_e(try_get_span(self.base, &e), "expected an atom"))?;
                self.dummies.push(a);
                vec.push(Mm0ExprNode::Const(e))
              }
              EType::Reg(_, _) => {
                let n = self.node(e)?;
                cnst &= matches!(n, Mm0ExprNode::Const(_));
                vec.push(n)
              }
            }
          }
          self.dummies.truncate(len);
          if cnst {None} else {Some(Mm0ExprNode::Expr(tid, vec))}
        } else {None})
      }

      fn node_opt(&mut self, e: &LispVal) -> Result<Option<Mm0ExprNode>> {
        e.unwrapped(|r| Ok(if let LispKind::Atom(a) = *r {
          if self.dummies.contains(&a) {return Ok(None)}
          match self.vars.entry(a) {
            Entry::Occupied(entry) => Some(Mm0ExprNode::Var(*entry.get())),
            Entry::Vacant(entry) => if let Some(v) = (self.get)(e, a) {
              let n = self.subst.len().try_into().expect("overflow");
              entry.insert(n);
              self.subst.push(v);
              Some(Mm0ExprNode::Var(n))
            } else {
              self.list_opt(e, a, None)?
            }
          }
        } else {
          let mut u = Uncons::from(e.clone());
          let head = u.next().ok_or_else(|| ElabError::new_e(try_get_span(self.base, e),
            format!("bad expression {}", self.p.fe.to(e))))?;
          let a = head.as_atom().ok_or_else(|| ElabError::new_e(try_get_span(self.base, &head),
            "expected an atom"))?;
          self.list_opt(&head, a, Some(u))?
        }))
      }

      #[allow(clippy::unnecessary_lazy_evaluations)]
      fn node(&mut self, e: LispVal) -> Result<Mm0ExprNode> {
        Ok(self.node_opt(&e)?.unwrap_or_else(|| Mm0ExprNode::Const(e)))
      }
    }

    let mut mm0 = Mm0 {
      subst: vec![],
      base,
      vars: HashMap::new(),
      dummies: vec![],
      get,
      p: self,
    };
    let expr = std::rc::Rc::new(mm0.node(e)?);
    Ok(Mm0Expr {subst: mm0.subst, expr})
  }
}
