//! `action_completion::apply` — output-tape executor for the tape-form
//! recipe model.
//!
//! Walks `recipe.output` top-to-bottom, accumulates state into a
//! [`TapeWalker`], and emits future-stamped card writes at
//! `completion_ms = start_ms + walker.duration * 1000`.
//!
//! State:
//! - `vars: [i32; 8]` — variable scratch (`var.N.set` / `add` / `sub`).
//! - `duration: u32` — written by `sys.duration.set`; consumed when
//!   emitting effect rows.
//! - `styles: BTreeMap<card_id, u8>` — populated by `<path>.style.set`;
//!   each entry becomes `progress_style` bits on that card's completion
//!   row. Replaces the implicit actor cascade.
//! - `pending: Vec<Effect>` — accumulated destroy / create effects.
//!
//! Dispatch (last segment of the path determines the op):
//!
//! | Op | Effect |
//! | --- | --- |
//! | `sys.duration.set` | Write `walker.duration`. |
//! | `<path>.style.set` | Stamp `progress_style` on the resolved card at completion. |
//! | `var.N.set / add / sub` | Variable arithmetic. Int value only today. |
//! | `when.<pred>.<inner>` | Eval predicate against vars; recurse into inner on match. |
//! | `<path>.destroy` | Queue destroy of resolved card. |
//! | `<path>.create: <def_id>` | Queue create at resolved destination. |
//!
//! **Not yet supported:** stock modify (`hex.aspect.X.sub`), variable
//! set with path-read RHS (`var.N.set: root.aspect.fleeting`),
//! `random`. Recipes using these execute partially; the unsupported
//! ops error with a clear message.
//!
//! **Lock release:** every bound card has its `slot_hold` cleared at
//! completion. Cards in `destroy` statements additionally get
//! `FLAG_DEAD` set. `position_hold` (ref-counted) is decremented for
//! every top-level chain card.

use std::collections::{BTreeMap, BTreeSet};

use resonantdust_content::definition_core::{
    aspect_id as core_aspect_id, decode_definition, find_packed_by_key, is_aspect_descendant,
};
use resonantdust_content::recipe_core::{Recipe, Seg, Stmt};
use resonantdust_content::recipe_statement::{parse_statement, Segment, StatementValue};
use spacetimedb::ReducerContext;

use crate::cards;
use crate::packed::INVENTORY_LAYER;
use crate::zones;

/// Address of a synthetic hex — a tile-as-hex resolved from Zone tile
/// bytes when the recipe's branch 0 has no card row backing it.
#[derive(Debug, Clone, Copy)]
pub struct HexLocation {
    pub zone_id: u32,
    pub macro_zone: u32,
    pub col: u8,
    pub row: u8,
    pub owner_id: u32,
}

pub const PROGRESS_STYLE_NONE: u32 = 0;
pub const PROGRESS_STYLE_LTR: u32 = 1;
pub const PROGRESS_STYLE_RTL: u32 = 2;

// Flag bits we manipulate on completion-time writes.
const FLAG_SLOT_HOLD: u32 = 1 << 5;
const FLAG_DEAD: u32 = 1 << 7;
const FLAG_FORCE_POSITION: u32 = 1 << 11;
const HOLD_RELEASE_MASK: u32 = FLAG_SLOT_HOLD | FLAG_FORCE_POSITION;

/// `progress_style` field — 3 bits at positions 8..=10 of
/// `cards.flags`. Source of truth: `content/cards/flags.json`.
const PROGRESS_STYLE_SHIFT: u32 = 8;
const PROGRESS_STYLE_MASK: u32 = 0b111 << PROGRESS_STYLE_SHIFT;

const VAR_SLOT_COUNT: usize = 8;

/// Per-action scratch state populated by the tape walker.
struct TapeWalker {
    vars: [i32; VAR_SLOT_COUNT],
    duration: u32,
    /// Cards that should carry a non-zero `progress_style` on their
    /// completion row, keyed by card_id. Populated by `<path>.style.set`
    /// output statements. Cards absent from this map get their
    /// `progress_style` field cleared at completion time. Replaces the
    /// implicit "actor" cascade (resolve_actor) with explicit per-card
    /// assignment, allowing multiple progress bars on a single recipe.
    styles: BTreeMap<u32, u8>,
    pending: Vec<Effect>,
}

/// Accumulated effect from walking the output tape. Emitted at
/// `completion_ms` after the walk is done.
enum Effect {
    Destroy {
        card_id: u32,
    },
    Create {
        def_key: String,
        surface: u8,
        macro_zone: u32,
        owner_id: u32,
    },
    /// Modify a stock slot on a tile inside a Zone row. Only
    /// synthetic-tile targets are supported today (the target path
    /// resolves to branch 0's no-card sentinel + a `HexLocation`).
    /// Real cards don't carry per-row stock yet.
    ModifyTileStock {
        zone_id: u32,
        row: u8,
        col: u8,
        slot: usize,
        op: StockOp,
        delta: u8,
    },
}

/// Stock-modify arithmetic. `Sub` and `Add` saturate at the u2
/// range (0..=3); `Set` writes the literal value clamped to that
/// range.
#[derive(Debug, Clone, Copy)]
enum StockOp {
    Sub,
    Add,
    Set,
}

impl TapeWalker {
    fn new() -> Self {
        Self {
            vars: [0; VAR_SLOT_COUNT],
            duration: 0,
            styles: BTreeMap::new(),
            pending: Vec::new(),
        }
    }
}

/// Walk `recipe.output` and emit completion-time card writes.
///
/// Computes `completion_ms = start_ms + walker.duration * 1000` from
/// the tape's `sys.duration.set` writes. All effects are emitted
/// future-stamped at that time so the rows become visible to
/// subscribers exactly when the action completes.
pub fn apply(
    ctx: &ReducerContext,
    recipe: &Recipe,
    bindings: &[Vec<u32>],
    root: u32,
    synthetic_hex: Option<HexLocation>,
    start_ms: u64,
    caller_player_id: u32,
) -> Result<(), String> {
    let _ = caller_player_id;

    let mut walker = TapeWalker::new();
    for (i, stmt) in recipe.output.iter().enumerate() {
        execute_stmt(
            ctx,
            &mut walker,
            recipe,
            stmt,
            bindings,
            root,
            synthetic_hex.as_ref(),
        )
        .map_err(|e| format!("output[{i}]: {e}"))?;
    }

    let completion_ms = start_ms + (walker.duration as u64) * 1000;

    // Emit effects at completion_ms.
    let mut consumed: BTreeSet<u32> = BTreeSet::new();
    for effect in &walker.pending {
        match effect {
            Effect::Destroy { card_id } => {
                consumed.insert(*card_id);
            }
            Effect::Create {
                def_key,
                surface,
                macro_zone,
                owner_id,
            } => {
                let packed_def = find_packed_by_key(def_key)
                    .map_err(|e| format!("create: find_packed_by_key({def_key:?}): {e}"))?
                    .ok_or_else(|| {
                        format!("create: def {def_key:?} not registered in cards/id.json")
                    })?;
                let new_id = cards::next_card_id(ctx);
                cards::create_at(
                    ctx,
                    new_id,
                    completion_ms,
                    *surface,
                    *macro_zone,
                    /* micro_zone     */ 0,
                    /* micro_location */ 0,
                    *owner_id,
                    packed_def,
                    /* flags          */ 0,
                );
            }
            Effect::ModifyTileStock {
                zone_id,
                row,
                col,
                slot,
                op,
                delta,
            } => {
                zones::update_with_at(ctx, *zone_id, completion_ms, |z| {
                    let current = z.tile_stock_at(*row, *col, *slot).unwrap_or(0);
                    let next = match op {
                        StockOp::Sub => current.saturating_sub(*delta),
                        StockOp::Add => current.saturating_add(*delta).min(0b11),
                        StockOp::Set => (*delta).min(0b11),
                    };
                    z.assign_tile_stock(*row, *col, *slot, next);
                });
            }
        }
    }

    // Release locks on every bound card (root + bindings). Consumed
    // cards (in `destroy` statements) get `FLAG_DEAD` set in the same
    // write; survivors just shed their `slot_hold` / `force_position`.
    // `position_hold` is ref-counted — we decrement for every card we
    // bumped on the propose side.
    //
    // `progress_style` is stamped per `walker.styles` — populated by
    // `<path>.style.set` output statements. Cards not in the map get
    // `progress_style` cleared so any previously-rendered bar
    // disappears at completion. The map drives multi-bar recipes
    // naturally: each style.set'd card carries its own value.
    let mut all_bound: BTreeSet<u32> = BTreeSet::new();
    if root != 0 {
        all_bound.insert(root);
    }
    for row in bindings {
        for &id in row {
            if id != 0 {
                all_bound.insert(id);
            }
        }
    }

    for &card_id in &all_bound {
        let is_consumed = consumed.contains(&card_id);
        let style = walker.styles.get(&card_id).copied().unwrap_or(0);
        let style_bits = (style as u32) << PROGRESS_STYLE_SHIFT;
        cards::update_with_at(ctx, card_id, completion_ms, |c| {
            if is_consumed {
                c.flags |= FLAG_DEAD;
            }
            c.flags &= !HOLD_RELEASE_MASK;
            c.flags &= !PROGRESS_STYLE_MASK;
            c.flags |= style_bits & PROGRESS_STYLE_MASK;
        });
        cards::release_position_hold(ctx, card_id, completion_ms);
    }

    Ok(())
}

// ----- Statement dispatch ---------------------------------------

#[allow(clippy::too_many_arguments)]
fn execute_stmt(
    ctx: &ReducerContext,
    walker: &mut TapeWalker,
    recipe: &Recipe,
    stmt: &Stmt,
    bindings: &[Vec<u32>],
    root: u32,
    synthetic_hex: Option<&HexLocation>,
) -> Result<(), String> {
    let segs = stmt.segments.as_slice();
    let head = match segs.first() {
        Some(Seg::Word(w)) => w.as_str(),
        Some(Seg::Slot { .. }) => {
            return execute_card_op(
                ctx,
                walker,
                recipe,
                stmt,
                bindings,
                root,
                synthetic_hex,
            );
        }
        Some(Seg::Index(_)) | None => {
            return Err(format!("malformed statement segments: {segs:?}"));
        }
    };

    match head {
        "when" => execute_when(ctx, walker, recipe, stmt, bindings, root, synthetic_hex),
        "sys" => execute_sys(walker, stmt),
        "var" => execute_var(ctx, walker, recipe, stmt, bindings, root, synthetic_hex),
        "root" => execute_card_op(ctx, walker, recipe, stmt, bindings, root, synthetic_hex),
        other => Err(format!("unsupported statement head {other:?}")),
    }
}

// ----- sys.X.set ------------------------------------------------

fn execute_sys(walker: &mut TapeWalker, stmt: &Stmt) -> Result<(), String> {
    let segs = stmt.segments.as_slice();
    // Expected: sys.<slot>.set
    if segs.len() != 3 {
        return Err(format!("sys: expected `sys.<slot>.set: <value>`, got {segs:?}"));
    }
    let slot = match &segs[1] {
        Seg::Word(w) => w.as_str(),
        _ => return Err("sys: second segment must be a slot name".to_string()),
    };
    match &segs[2] {
        Seg::Word(w) if w == "set" => {}
        other => return Err(format!("sys: third segment must be `set`, got {other:?}")),
    }
    match slot {
        "duration" => {
            let n = match &stmt.value {
                Some(StatementValue::Int(n)) => *n as u32,
                _ => return Err("sys.duration.set: requires integer value".to_string()),
            };
            walker.duration = n;
        }
        other => return Err(format!("sys: unknown slot {other:?}")),
    }
    Ok(())
}

/// Decode a style value from the statement's `value` payload. Accepts
/// the named strings (`"ltr"` / `"rtl"` / `"none"`) and raw integer
/// codes (0..=7). Used by `<path>.style.set` to populate
/// `walker.styles`.
fn style_from_value(value: &Option<StatementValue>) -> Result<u8, String> {
    match value {
        Some(StatementValue::Str(s)) => match s.as_str() {
            "none" => Ok(PROGRESS_STYLE_NONE as u8),
            "ltr" => Ok(PROGRESS_STYLE_LTR as u8),
            "rtl" => Ok(PROGRESS_STYLE_RTL as u8),
            other => Err(format!("style.set: unknown style {other:?}")),
        },
        Some(StatementValue::Int(n)) => Ok((*n as u8) & 0b111),
        None => Err("style.set: requires a value".to_string()),
    }
}

// ----- var.N.set / add / sub ------------------------------------

#[allow(clippy::too_many_arguments)]
fn execute_var(
    ctx: &ReducerContext,
    walker: &mut TapeWalker,
    recipe: &Recipe,
    stmt: &Stmt,
    bindings: &[Vec<u32>],
    root: u32,
    synthetic_hex: Option<&HexLocation>,
) -> Result<(), String> {
    let segs = stmt.segments.as_slice();
    if segs.len() != 3 {
        return Err(format!("var: expected `var.N.<op>: <value>`, got {segs:?}"));
    }
    let var_idx = match &segs[1] {
        Seg::Index(n) => *n as usize,
        _ => return Err("var: second segment must be a variable index".to_string()),
    };
    if var_idx >= VAR_SLOT_COUNT {
        return Err(format!(
            "var.{var_idx}: index out of range (max {})",
            VAR_SLOT_COUNT - 1
        ));
    }
    let op = match &segs[2] {
        Seg::Word(w) => w.as_str(),
        _ => return Err("var: third segment must be an op word".to_string()),
    };
    // Resolve the operand. `Int` is a literal; `Str` is a path
    // expression read at runtime (e.g. `root.aspect.fleeting`).
    let operand = match &stmt.value {
        Some(StatementValue::Int(n)) => *n as i32,
        Some(StatementValue::Str(path_str)) => {
            read_path_value(ctx, recipe, path_str, bindings, root, synthetic_hex)
                .map_err(|e| format!("var.{var_idx}.{op}: path-RHS read {path_str:?}: {e}"))?
        }
        None => return Err(format!("var.{var_idx}.{op}: requires a value")),
    };
    match op {
        "set" => walker.vars[var_idx] = operand,
        "add" => walker.vars[var_idx] = walker.vars[var_idx].saturating_add(operand),
        "sub" => walker.vars[var_idx] = walker.vars[var_idx].saturating_sub(operand),
        other => return Err(format!("var.{var_idx}: unsupported op {other:?}")),
    }
    Ok(())
}

// ----- when.<predicate>.<inner_statement> ----------------------

#[allow(clippy::too_many_arguments)]
fn execute_when(
    ctx: &ReducerContext,
    walker: &mut TapeWalker,
    recipe: &Recipe,
    stmt: &Stmt,
    bindings: &[Vec<u32>],
    root: u32,
    synthetic_hex: Option<&HexLocation>,
) -> Result<(), String> {
    let segs = stmt.segments.as_slice();
    // Expected shape: [when, <pred-path>..., <cmp-op>, <value>, <inner-stmt>...]
    // pred-path is `var.N` for the form we support today.
    // cmp-op is one of gt/ge/lt/le/eq/ne.
    let cmp_idx = (1..segs.len()).find(|&i| match &segs[i] {
        Seg::Word(w) => matches!(
            w.as_str(),
            "gt" | "ge" | "lt" | "le" | "eq" | "ne"
        ),
        _ => false,
    });
    let cmp_idx = cmp_idx.ok_or_else(|| {
        format!("when: no comparison op (gt/ge/lt/le/eq/ne) found in {segs:?}")
    })?;
    if cmp_idx + 1 >= segs.len() {
        return Err("when: comparison op needs a value segment after it".to_string());
    }

    // Predicate path (between `when` and the comparison op).
    let pred_path = &segs[1..cmp_idx];
    let cmp_op = match &segs[cmp_idx] {
        Seg::Word(w) => w.as_str(),
        _ => unreachable!(),
    };
    let cmp_value = match &segs[cmp_idx + 1] {
        Seg::Index(n) => *n as i32,
        Seg::Word(_) => {
            return Err("when: comparison value must be an integer".to_string());
        }
        Seg::Slot { .. } => {
            return Err("when: comparison value cannot be a slot ref".to_string());
        }
    };
    let inner_segs = segs[cmp_idx + 2..].to_vec();
    if inner_segs.is_empty() {
        return Err("when: missing inner statement after predicate".to_string());
    }

    // Resolve predicate path to an i32. Today only `var.N` is
    // supported; aspect / path reads come later.
    let pred_value = match pred_path {
        [Seg::Word(w), Seg::Index(n)] if w == "var" => {
            *walker.vars.get(*n as usize).ok_or_else(|| {
                format!("when: var.{n} out of range")
            })?
        }
        other => {
            return Err(format!(
                "when: predicate path must be `var.N` today; got {other:?}"
            ));
        }
    };

    let matched = match cmp_op {
        "gt" => pred_value > cmp_value,
        "ge" => pred_value >= cmp_value,
        "lt" => pred_value < cmp_value,
        "le" => pred_value <= cmp_value,
        "eq" => pred_value == cmp_value,
        "ne" => pred_value != cmp_value,
        _ => unreachable!(),
    };

    if !matched {
        return Ok(());
    }

    let inner_stmt = Stmt {
        segments: inner_segs,
        value: stmt.value.clone(),
        slot_hold: stmt.slot_hold,
        position_hold: stmt.position_hold,
    };
    execute_stmt(ctx, walker, recipe, &inner_stmt, bindings, root, synthetic_hex)
}

// ----- card-side ops: destroy / create -------------------------

#[allow(clippy::too_many_arguments)]
fn execute_card_op(
    ctx: &ReducerContext,
    walker: &mut TapeWalker,
    recipe: &Recipe,
    stmt: &Stmt,
    bindings: &[Vec<u32>],
    root: u32,
    synthetic_hex: Option<&HexLocation>,
) -> Result<(), String> {
    let segs = stmt.segments.as_slice();
    let op = match segs.last() {
        Some(Seg::Word(w)) => w.as_str(),
        _ => return Err(format!("card op: last segment must be a word; got {segs:?}")),
    };
    let path = &segs[..segs.len() - 1];

    match op {
        "destroy" => {
            let card_id = resolve_card_target(ctx, recipe, path, bindings, root)?;
            walker.pending.push(Effect::Destroy { card_id });
            Ok(())
        }
        "create" => {
            // Path ends in `.inventory` — the destination is the
            // resolved card's inventory bucket.
            if path.last().and_then(|s| match s {
                Seg::Word(w) => Some(w.as_str()),
                _ => None,
            }) != Some("inventory")
            {
                return Err(format!(
                    "create: path must end in `.inventory.create`; got {segs:?}"
                ));
            }
            let target_path = &path[..path.len() - 1];
            let owner_card_id =
                resolve_card_target(ctx, recipe, target_path, bindings, root)?;
            let def_key = match &stmt.value {
                Some(StatementValue::Str(s)) => s.clone(),
                _ => return Err("create: requires a string def_id value".to_string()),
            };
            walker.pending.push(Effect::Create {
                def_key,
                surface: INVENTORY_LAYER,
                macro_zone: owner_card_id,
                owner_id: owner_card_id,
            });
            Ok(())
        }
        "set" if matches!(path.last(), Some(Seg::Word(w)) if w == "style") => {
            // `<path>.style.set: <value>` — stamp `progress_style` on
            // the resolved card's completion row. The target path is
            // everything before `.style`; the value is decoded via
            // `style_from_value` (named strings or raw 0..=7 int).
            //
            // Recipes can emit multiple `style.set` statements to put
            // bars on multiple cards. Cards without a `style.set`
            // entry get `progress_style` cleared at completion.
            // Replaces the implicit "actor" cascade (resolve_actor).
            let target_path = &path[..path.len() - 1];
            let target_id = resolve_card_target(ctx, recipe, target_path, bindings, root)?;
            let style = style_from_value(&stmt.value)?;
            walker.styles.insert(target_id, style);
            Ok(())
        }
        "sub" | "add" | "set" => {
            // Stock-modify shape: `<path>.aspect.<name>.<op>: <N>`
            // segments = [<target...>, aspect, <name>, <op>]
            //
            // For v1 only the synthetic-tile case is supported —
            // target must resolve to branch 0's no-card sentinel
            // with a HexLocation provided. Real cards don't carry
            // mutable per-row stocks today, so a `<path>.aspect.X.sub`
            // against a card is a content-authoring error.
            if path.len() < 2 {
                return Err(format!("stock modify: short path {segs:?}"));
            }
            let aspect_word = match &path[path.len() - 2] {
                Seg::Word(w) if w == "aspect" => w,
                other => {
                    return Err(format!(
                        "stock modify: expected `.aspect.<name>.<op>`; got {other:?}"
                    ))
                }
            };
            let _ = aspect_word;
            let aspect_name = match &path[path.len() - 1] {
                Seg::Word(w) => w.as_str(),
                other => {
                    return Err(format!(
                        "stock modify: aspect name must be a word; got {other:?}"
                    ))
                }
            };
            let delta = match &stmt.value {
                Some(StatementValue::Int(n)) => (*n).max(0).min(255) as u8,
                _ => return Err("stock modify: requires an integer value".to_string()),
            };
            let target_path = &path[..path.len() - 2];
            // Resolve the target. For v1, the target must be the
            // synthetic-tile sentinel — single Slot ref to branch
            // 0, offset 0, with no further chain.
            let synth = require_synthetic_tile_target(recipe, target_path, synthetic_hex)?;
            // Look up the aspect's numeric id, then find the tile
            // def's stock slot that matches it.
            let aspect_id_val = core_aspect_id(aspect_name)
                .map_err(|e| format!("aspect lookup {aspect_name:?}: {e}"))?
                .ok_or_else(|| format!("unknown aspect {aspect_name:?}"))?;
            // Read the tile's current def from the zone to find
            // the stock slot index.
            let zone = zones::latest(ctx, synth.zone_id).ok_or_else(|| {
                format!("zone {} not found", synth.zone_id)
            })?;
            let (def_id, _, _) = zone
                .tile_at(synth.row, synth.col)
                .ok_or_else(|| format!("no tile at ({},{})", synth.col, synth.row))?;
            const TILE_CARD_TYPE: u8 = 7;
            let packed_def = crate::packed::pack_definition(TILE_CARD_TYPE, def_id);
            let tile_def = decode_definition(packed_def)
                .map_err(|e| format!("decode tile def: {e}"))?
                .ok_or_else(|| format!("tile packed {packed_def:#06x} has no def"))?;
            // Find a stock slot whose declared aspect descends from
            // (or equals) the requested aspect id. Use the first
            // such slot — for fine-grained control authors should
            // use a leaf aspect name directly.
            let slot_idx = tile_def
                .stock
                .iter()
                .position(|s| is_aspect_descendant(s.aspect_id, aspect_id_val).unwrap_or(false))
                .ok_or_else(|| {
                    format!(
                        "stock modify: tile def {:?} declares no stock slot for aspect {aspect_name:?}",
                        tile_def.key
                    )
                })?;
            let stock_op = match op {
                "sub" => StockOp::Sub,
                "add" => StockOp::Add,
                "set" => StockOp::Set,
                _ => unreachable!(),
            };
            walker.pending.push(Effect::ModifyTileStock {
                zone_id: synth.zone_id,
                row: synth.row,
                col: synth.col,
                slot: slot_idx,
                op: stock_op,
                delta,
            });
            Ok(())
        }
        other => Err(format!("card op: unsupported op {other:?}")),
    }
}

/// Confirm the target path resolves to the synthetic tile
/// (branch 0 / offset 0 / no chain navigation). Used by stock-
/// modify ops since real cards don't carry mutable per-row stocks
/// today.
fn require_synthetic_tile_target<'a>(
    recipe: &Recipe,
    path: &[Seg],
    synthetic_hex: Option<&'a HexLocation>,
) -> Result<&'a HexLocation, String> {
    let synth = synthetic_hex.ok_or_else(|| {
        "stock modify: requires a synthetic tile (no `HexLocation` resolved at propose time)"
            .to_string()
    })?;
    if path.len() != 1 {
        return Err(format!(
            "stock modify v1: target must be a single slot ref (no chain navigation); got {path:?}"
        ));
    }
    match &path[0] {
        Seg::Slot {
            iterator_id,
            offset,
        } => {
            let it = recipe
                .iterators
                .get(*iterator_id as usize)
                .ok_or_else(|| format!("iterator_id {iterator_id} out of range"))?;
            if !it.parent.is_empty() || it.branch != 0 || *offset != 0 {
                return Err(format!(
                    "stock modify v1: target must be branch 0 / offset 0 (synthetic tile)"
                ));
            }
            Ok(synth)
        }
        other => Err(format!(
            "stock modify v1: target must be a Slot ref; got {other:?}"
        )),
    }
}

// ----- path resolution (mirrors actions.rs::resolve_target) -----

/// Resolve a path to its terminal `card_id`. Path segments:
/// - `root` — the action's root card.
/// - `Slot { iter, offset }` — direct binding index.
/// - `.owner` — follow `card.owner_id`.
/// - `.parent` — follow `card.micro_location`.
///
/// This is a runtime-side mirror of the predicate-time resolver in
/// `actions.rs::resolve_target`, minus the transition checks (those
/// already happened in Stage 1 verification at propose time).
fn resolve_card_target(
    ctx: &ReducerContext,
    recipe: &Recipe,
    path: &[Seg],
    bindings: &[Vec<u32>],
    root: u32,
) -> Result<u32, String> {
    let mut card_id = match path.first() {
        Some(Seg::Word(w)) if w == "root" => {
            if root == 0 {
                return Err("resolve: root is 0".to_string());
            }
            root
        }
        Some(Seg::Slot {
            iterator_id,
            offset,
        }) => {
            let binding_row = bindings
                .get(*iterator_id as usize)
                .ok_or_else(|| format!("bindings missing iterator {iterator_id}"))?;
            *binding_row.get(*offset as usize).ok_or_else(|| {
                format!(
                    "iterator {iterator_id} offset {offset} out of range \
                     (binding len {})",
                    binding_row.len()
                )
            })?
        }
        other => return Err(format!("resolve: unsupported anchor {other:?}")),
    };
    let _ = recipe;

    let mut i = 1;
    while i < path.len() {
        match &path[i] {
            Seg::Word(w) if w == "owner" => {
                let card = cards::latest(ctx, card_id)
                    .ok_or_else(|| format!("resolve: card {card_id} not found"))?;
                if card.owner_id == 0 {
                    return Err(format!("resolve: card {card_id} has no owner"));
                }
                card_id = card.owner_id;
                i += 1;
            }
            Seg::Word(w) if w == "parent" => {
                let card = cards::latest(ctx, card_id)
                    .ok_or_else(|| format!("resolve: card {card_id} not found"))?;
                if card.micro_location == 0 {
                    return Err(format!("resolve: card {card_id} has no parent"));
                }
                card_id = card.micro_location;
                i += 1;
            }
            Seg::Slot {
                iterator_id,
                offset,
            } => {
                let binding_row = bindings
                    .get(*iterator_id as usize)
                    .ok_or_else(|| format!("bindings missing iterator {iterator_id}"))?;
                card_id = *binding_row.get(*offset as usize).ok_or_else(|| {
                    format!("iterator {iterator_id} offset {offset} out of range")
                })?;
                i += 1;
            }
            other => {
                return Err(format!(
                    "resolve: unsupported path segment {other:?}"
                ))
            }
        }
    }
    if card_id == 0 {
        return Err("resolve: terminal card_id is 0".to_string());
    }
    Ok(card_id)
}

/// Walk raw `Segment`s left-to-right and collapse every
/// `[Word("slot"), Index(B), Index(N)]` triplet into a single
/// `Seg::Slot` by matching the recipe's existing iterators (read-only
/// — never adds new ones). The matched iterator must have
/// `parent == out_so_far` and `branch == B`; if no match exists the
/// recipe didn't declare this slot reference up front and resolution
/// fails.
///
/// Used at runtime by paths that come from string values (e.g.
/// `var.0.set: slot.2.0.aspect.wood`) — those skip the recipe-parser's
/// slot-resolution pass. Non-slot segments pass through as `Seg::Word`
/// / `Seg::Index`.
fn collapse_slots_readonly(
    raw: &[Segment],
    iterators: &[resonantdust_content::recipe_core::Iterator],
) -> Result<Vec<Seg>, String> {
    let mut out: Vec<Seg> = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        if i + 2 < raw.len() {
            if raw[i].as_word() == Some("slot") {
                if let (Some(branch_u32), Some(offset)) =
                    (raw[i + 1].as_index(), raw[i + 2].as_index())
                {
                    if branch_u32 > 255 {
                        return Err(format!(
                            "slot.{branch_u32}.{offset}: branch must fit in u8"
                        ));
                    }
                    let branch = branch_u32 as u8;
                    let parent_slice: &[Seg] = &out;
                    let iter_id = iterators
                        .iter()
                        .position(|it| {
                            it.parent.as_slice() == parent_slice
                                && it.branch == branch
                        })
                        .ok_or_else(|| {
                            format!(
                                "slot.{branch}.{offset}: no iterator in recipe with parent={parent_slice:?} branch={branch}"
                            )
                        })?;
                    out.push(Seg::Slot {
                        iterator_id: iter_id as u32,
                        offset,
                    });
                    i += 3;
                    continue;
                }
            }
        }
        out.push(match &raw[i] {
            Segment::Word(w) => Seg::Word(w.clone()),
            Segment::Index(n) => Seg::Index(*n),
        });
        i += 1;
    }
    Ok(out)
}

/// Read a value at runtime from a path expression. Used by
/// `var.N.set/add/sub` when the RHS is a string (e.g.
/// `var.0.set: root.aspect.fleeting`).
///
/// Supported terminals:
/// - `<card-path>.aspect.<name>` — read the aspect's value from
///   the resolved card's def. Static aspects only today; sub-aspect
///   widening + row-mutable stocks come later.
///
/// The card-path portion is resolved with the same machinery as
/// destroy / create targets (anchor + `.owner` / `.parent` chain
/// steps + Slot refs).
fn read_path_value(
    ctx: &ReducerContext,
    recipe: &Recipe,
    path_str: &str,
    bindings: &[Vec<u32>],
    root: u32,
    synthetic_hex: Option<&HexLocation>,
) -> Result<i32, String> {
    // Re-parse the path string into segments. We append a fake
    // value so the statement parser doesn't reject a bare path
    // (it expects either `<path>` or `<path>: <value>`).
    let raw = parse_statement(path_str)
        .map_err(|e| format!("parse path: {e}"))?;
    let segs = &raw.path;
    if segs.len() < 3 {
        return Err(format!(
            "read_path_value: path too short for `.aspect.<name>` terminal: {path_str:?}"
        ));
    }
    let aspect_word = match &segs[segs.len() - 2] {
        Segment::Word(w) if w == "aspect" => w,
        other => {
            return Err(format!(
                "read_path_value: expected `.aspect.<name>` terminal; got {other:?}"
            ))
        }
    };
    let _ = aspect_word;
    let aspect_name = match &segs[segs.len() - 1] {
        Segment::Word(w) => w.as_str(),
        other => {
            return Err(format!(
                "read_path_value: aspect name must be a word; got {other:?}"
            ))
        }
    };
    let aspect_id_val = core_aspect_id(aspect_name)
        .map_err(|e| format!("aspect lookup {aspect_name:?}: {e}"))?
        .ok_or_else(|| format!("unknown aspect {aspect_name:?}"))?;

    // Resolve the card-path prefix (everything before
    // `.aspect.<name>`). The path string was parsed by the
    // statement parser, which doesn't collapse `slot.B.N` into
    // `Seg::Slot` (that's recipe_tape's job and only runs on the
    // recipe's input/output arrays at recipe-parse time, not on
    // variable-RHS strings parsed at runtime). We do the
    // equivalent collapse here against the recipe's already-resolved
    // iterators (read-only — never add new ones) and hand the
    // resulting Seg-form path to `resolve_card_target`, which knows
    // how to walk slot refs + `.owner` / `.parent` chain steps.
    let card_path = &segs[..segs.len() - 2];
    let resolved_path = collapse_slots_readonly(card_path, &recipe.iterators)?;
    let card_id =
        resolve_card_target(ctx, recipe, &resolved_path, bindings, root)?;
    let _ = synthetic_hex;

    let card = cards::latest(ctx, card_id)
        .ok_or_else(|| format!("read_path_value: card {card_id} not found"))?;
    let def = decode_definition(card.packed_definition)
        .map_err(|e| format!("decode def: {e}"))?
        .ok_or_else(|| format!("card {card_id} has unknown def"))?;
    // Sum static aspect values that descend from (or equal) the
    // target aspect id. Mirrors the server-side predicate matcher's
    // sub-aspect widening: a `food` read on a card carrying
    // `berries` returns the berries value.
    let mut total: i32 = 0;
    for (id, v) in &def.aspects {
        if is_aspect_descendant(*id, aspect_id_val).unwrap_or(false) {
            total += *v;
        }
    }
    Ok(total)
}
