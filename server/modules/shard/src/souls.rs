
use spacetimedb::{table, ReducerContext, Table};

use crate::cards::Card;
use crate::packed::{pack_valid_at, valid_at_time};
use crate::sequence;

// NOTE: per-soul portrait selection was dropped from the card flag word in the
// flags/stock schema split (portrait_id no longer fits the propagating `flags`
// u32). It needs to be relocated to soul data (a new per-soul field or derived
// from def_id) before soul portraits render again. TODO(portrait-relocate).

/// Soul row — one per soul card, public so clients can subscribe.
///
/// Positional fields (`macro_zone`, `micro_location`) mirror the soul
/// card itself; they're kept in sync by the `on_card_write` hook that
/// fires from `cards::write_at` on every card insert. Souls are loose on
/// the world surface, so `micro_location` carries packed loose coords
/// (`micro_is_card` is clear). Stats / fatigued / injured fields each pack four
/// `u8` counters into a `u32` (byte 0 = corpus, 1 = anima,
/// 2 = sollertia, 3 = aether — same order across all three fields):
///
/// - `stats`    — counts of `corpus` / `anima` / `sollertia` / `aether`
///   faculty cards currently owned by this soul's player.
/// - `fatigued` — counts of the `-` variants
///   (`corpus-` / `anima-` / `sollertia-` / `aether-`).
/// - `injured`  — counts of the `-i` variants (reserved; mapping is
///   empty until the injured-faculty cards are added to content).
///
/// Counts saturate at `u8::MAX` (255) — a soul accumulating more than
/// that many of a single faculty is well outside design space, but the
/// `saturating_add` keeps the diff hook honest in the pathological
/// case.
#[table(accessor = souls, public)]
#[derive(Debug, Clone)]
pub struct Soul {
    /// Packed primary key — `[time_ms: u48 | seq: u16]`. Same
    /// history-row pattern `cards` / `players` / `zones` use. The
    /// `card_id` is on the row column (see below); the row with the
    /// largest `valid_at_time` is the active state.
    #[primary_key]
    pub valid_at: u64,
    #[index(btree)]
    pub card_id: u32,
    #[index(btree)]
    pub owner_id: u32,
    pub macro_zone: u64,
    pub micro_location: u32,
    /// `[corpus | anima | sollertia | aether]` packed little-endian.
    pub stats: u32,
    /// `[corpus- | anima- | sollertia- | aether-]` packed little-endian.
    pub fatigued: u32,
    /// `[corpus-i | anima-i | sollertia-i | aether-i]` packed
    /// little-endian. Counters stay 0 until the `-i` faculty cards
    /// exist in content and `stat_slot_for` maps them in.
    pub injured: u32,
}

// ---- u32 quad packing -----------------------------------------------

/// Pack four `u8`s into a `u32` in the same order they're laid out in
/// `stats` / `fatigued` / `injured`. Little-endian: byte `i` occupies
/// bits `i*8 .. i*8 + 8`.
pub fn pack_quad(b0: u8, b1: u8, b2: u8, b3: u8) -> u32 {
    (b0 as u32) | ((b1 as u32) << 8) | ((b2 as u32) << 16) | ((b3 as u32) << 24)
}

/// Inverse of [`pack_quad`].
pub fn unpack_quad(v: u32) -> (u8, u8, u8, u8) {
    (
        v as u8,
        (v >> 8) as u8,
        (v >> 16) as u8,
        (v >> 24) as u8,
    )
}

/// Read one byte (`byte_index` 0..=3) out of a packed quad.
pub fn quad_get(v: u32, byte_index: u8) -> u8 {
    (v >> (byte_index as u32 * 8)) as u8
}

/// Replace one byte (`byte_index` 0..=3) inside a packed quad.
pub fn quad_set(v: u32, byte_index: u8, byte: u8) -> u32 {
    let shift = byte_index as u32 * 8;
    let mask = !(0xFF_u32 << shift);
    (v & mask) | ((byte as u32) << shift)
}

// ---- CRUD ----------------------------------------------------------

pub fn latest(ctx: &ReducerContext, card_id: u32) -> Option<Soul> {
    ctx.db
        .souls()
        .card_id()
        .filter(card_id)
        .max_by_key(|s| valid_at_time(s.valid_at))
}

fn write_at(ctx: &ReducerContext, mut soul: Soul, time_ms: u64) -> Soul {
    // "Last write at this (card_id, time_ms) wins." The new sequence-
    // bearing PK is unique per write, so without this purge an
    // in-reducer accumulator like `apply_slot_delta` firing 3 times
    // at `now_ms` (bootstrap's 3-corpus path) would leave 3 distinct
    // soul rows at the same time, each with a different stats value,
    // and `latest()` would pick whichever max_by_key returns on ties
    // — non-deterministic and almost always wrong.
    let stale: Vec<u64> = ctx
        .db
        .souls()
        .card_id()
        .filter(soul.card_id)
        .filter(|s| valid_at_time(s.valid_at) == time_ms)
        .map(|s| s.valid_at)
        .collect();
    for v in stale {
        ctx.db.souls().valid_at().delete(v);
    }
    soul.valid_at = pack_valid_at(time_ms, sequence::next_sequence(ctx));
    let inserted = ctx.db.souls().insert(soul);
    // No per-write delete schedule — `crate::gc` handles
    // prior-version reap on its periodic sweep.
    inserted
}

pub fn update_with_at<F>(
    ctx: &ReducerContext,
    card_id: u32,
    time_ms: u64,
    f: F,
) -> Option<Soul>
where
    F: FnOnce(&mut Soul),
{
    let mut s = latest(ctx, card_id)?;
    f(&mut s);
    Some(write_at(ctx, s, time_ms))
}

// ---- stat-slot mapping ---------------------------------------------

/// Which packed `u32` field on `Soul` a counter lives in. The byte
/// index inside that field is carried separately on [`StatSlot`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatField {
    Stats,
    Fatigued,
    #[allow(dead_code)] // wired into the table but no card defs map here yet
    Injured,
}

/// Address of a single stat counter inside a Soul row: the field
/// (`stats` / `fatigued` / `injured`) and the byte index within it
/// (0 = corpus, 1 = anima, 2 = sollertia, 3 = aether).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatSlot {
    pub field: StatField,
    pub byte_index: u8,
}

impl StatField {
    /// Decode the wire `field` selector the gate's `set_soul_stat` passes
    /// (`0 = stats`, `1 = fatigued`, `2 = injured`).
    pub fn from_u8(v: u8) -> Option<StatField> {
        match v {
            0 => Some(StatField::Stats),
            1 => Some(StatField::Fatigued),
            2 => Some(StatField::Injured),
            _ => None,
        }
    }
}

/// Apply a `±delta` to one Soul stat slot — the content-agnostic byte write the
/// gate drives via `set_soul_stat`. The stat-card → `(field, byte)` mapping (and
/// the decision *when* a stat changed) now lives in the gate; this just mutates
/// the bytes. `field`: 0=stats, 1=fatigued, 2=injured. Replaces the old
/// per-card-write `stat_map` diff (which read the content registry on every
/// write — plan `01_gate_authority_pivot`).
pub fn apply_stat(ctx: &ReducerContext, soul_card_id: u32, field: u8, byte_index: u8, delta: i8, time_ms: u64) {
    let Some(field) = StatField::from_u8(field) else { return };
    apply_slot_delta(ctx, soul_card_id, StatSlot { field, byte_index }, delta, time_ms);
}


// ---- soul-card identity --------------------------------------------

pub fn is_soul_card(packed_def: u16) -> bool {
    ((packed_def >> 12) & 0xF) as u8 == resonantdust_codec::packed::SOUL_CARD_TYPE
}

// ---- the hook ------------------------------------------------------

/// Called from `cards::write_at` after every card insert. Maintains
/// the `Soul` table in lockstep with the cards table:
///
/// 1. **Soul-card identity sync.** If the written card is a soul
///    (`card_type == 6`), ensures a `Soul` row exists for its
///    `card_id` and mirrors the soul card's positional fields and
///    owner_id onto it. Auto-creates the row on first sight, so
///    `players::spawn_soul_for` doesn't need to manually manage Soul
///    rows — they appear as a side effect of writing the soul card.
///
/// The hook is no-op for cards that aren't soul cards.
pub fn on_card_write(
    ctx: &ReducerContext,
    new_card: &Card,
    time_ms: u64,
) {
    // (1) Identity sync. Only fires when the written card is a soul
    // card. Reads the *prior* soul row (max valid_at_time ≤ time_ms)
    // for the same reason `apply_slot_delta` does — out-of-order
    // writes within a reducer (e.g., movement followed by an earlier-
    // stamped action completion) must not contaminate older rows
    // with newer positional state. Auto-creates the Soul row on the
    // very first write of a given soul card.
    //
    // Future positional changes (later soul rows from movement) are
    // intentionally untouched here: each move_card step has its own
    // (surface, macro_zone, ...) snapshot that we mustn't clobber.
    // The "stop on first deliberate change" forward-propagation rule
    // (used by zones) would apply if we ever did want to forward-
    // sync positional fields too.
    if is_soul_card(new_card.packed_definition) {
        let prior = ctx
            .db
            .souls()
            .card_id()
            .filter(new_card.card_id)
            .filter(|s| valid_at_time(s.valid_at) <= time_ms)
            .max_by_key(|s| valid_at_time(s.valid_at));
        match prior {
            Some(mut s) => {
                s.owner_id = new_card.owner_id;
                s.macro_zone = new_card.macro_zone;
                s.micro_location = new_card.micro_location;
                write_at(ctx, s, time_ms);
            }
            None => {
                write_at(
                    ctx,
                    Soul {
                        valid_at: 0,
                        card_id: new_card.card_id,
                        owner_id: new_card.owner_id,
                        macro_zone: new_card.macro_zone,
                        micro_location: new_card.micro_location,
                        stats: 0,
                        fatigued: 0,
                        injured: 0,
                    },
                    time_ms,
                );
            }
        }
    }

    // Stat-counter diff: REMOVED. Mapping a card's packed_def to a stat
    // slot needs the content registry, which no longer lives on the server.
    // The gate now owns soul-stats — it pushes `set_soul_stat` deltas when it
    // creates/destroys (or moves) a stat card (plan `01_gate_authority_pivot`,
    // P3). This hook keeps only the content-free identity-sync branch.
}

/// Apply a `±1` to one stat slot on the Soul row keyed by
/// `soul_card_id`. Silently skips when the soul has no row yet.
///
/// Two pieces of bookkeeping that mirror the zones forward-propagation
/// fix — both essential when card writes inside one reducer happen
/// out of chronological order (which they do: `action_completion::apply`
/// writes immediate rows, calls `on_create::trigger` on its products
/// which writes future-stamped rows, then returns to the outer apply
/// to write more immediate rows):
///
/// 1. **Read from the prior soul row, not the latest.** "Prior" is
///    the row with max `valid_at_time ≤ time_ms`. Reading
///    `latest()` (unbounded max) would pull in deltas applied by
///    later writes, contaminating our row with not-yet-due state.
///
/// 2. **Forward-propagate the delta.** Every soul row with
///    `valid_at_time > time_ms` represents the soul's state at
///    that future moment — and *that* state needs to include our
///    delta too, because the card we just touched is alive (or
///    newly-consumed) from `time_ms` onward. Unlike zones'
///    forward-propagation (which stops on deliberate change), souls
///    always propagate: each delta is an independent card-state
///    contribution, and downstream writes have already computed their
///    own deltas from card rows (not from the soul row), so we
///    aren't "double-counting" anything.
fn apply_slot_delta(
    ctx: &ReducerContext,
    soul_card_id: u32,
    slot: StatSlot,
    delta: i8,
    time_ms: u64,
) {
    if soul_card_id == 0 {
        return;
    }

    // (1) Read the prior soul row. Bail if no Soul exists yet — would
    // happen if a stat card is somehow written before the soul card
    // creates the Soul row via the identity-sync branch of
    // `on_card_write`. Today's call order guarantees the soul card is
    // always created before any owned faculty cards, but defending
    // here keeps the function honest.
    let Some(mut prior) = ctx
        .db
        .souls()
        .card_id()
        .filter(soul_card_id)
        .filter(|s| valid_at_time(s.valid_at) <= time_ms)
        .max_by_key(|s| valid_at_time(s.valid_at))
    else {
        return;
    };
    apply_delta_to_soul(&mut prior, slot, delta);
    write_at(ctx, prior, time_ms);

    // (2) Forward-propagate. Collect future row valid_ats first so
    // we're not iterating the table while mutating it.
    let mut future: Vec<u64> = ctx
        .db
        .souls()
        .card_id()
        .filter(soul_card_id)
        .filter(|s| valid_at_time(s.valid_at) > time_ms)
        .map(|s| s.valid_at)
        .collect();
    future.sort_unstable_by_key(|v| valid_at_time(*v));
    for v in future {
        let Some(mut s) = ctx.db.souls().valid_at().find(v) else {
            continue;
        };
        apply_delta_to_soul(&mut s, slot, delta);
        ctx.db.souls().valid_at().delete(v);
        ctx.db.souls().insert(s);
    }
}

/// Apply a signed `±1` delta to one byte of one packed-quad field on
/// `Soul`. Saturates at `[0, u8::MAX]`. Extracted as a helper because
/// `apply_slot_delta` uses it twice — once on the prior-row mutation,
/// once per future row during forward-propagation.
fn apply_delta_to_soul(s: &mut Soul, slot: StatSlot, delta: i8) {
    let field_value = match slot.field {
        StatField::Stats => s.stats,
        StatField::Fatigued => s.fatigued,
        StatField::Injured => s.injured,
    };
    let cur = quad_get(field_value, slot.byte_index);
    let next = if delta > 0 {
        cur.saturating_add(1)
    } else {
        cur.saturating_sub(1)
    };
    let updated = quad_set(field_value, slot.byte_index, next);
    match slot.field {
        StatField::Stats => s.stats = updated,
        StatField::Fatigued => s.fatigued = updated,
        StatField::Injured => s.injured = updated,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quad_pack_round_trip() {
        let v = pack_quad(1, 2, 3, 4);
        assert_eq!(unpack_quad(v), (1, 2, 3, 4));
    }

    #[test]
    fn quad_get_set() {
        let v = pack_quad(10, 20, 30, 40);
        assert_eq!(quad_get(v, 0), 10);
        assert_eq!(quad_get(v, 1), 20);
        assert_eq!(quad_get(v, 2), 30);
        assert_eq!(quad_get(v, 3), 40);
        let v2 = quad_set(v, 1, 99);
        assert_eq!(quad_get(v2, 1), 99);
        assert_eq!(quad_get(v2, 0), 10);
        assert_eq!(quad_get(v2, 2), 30);
    }
}
