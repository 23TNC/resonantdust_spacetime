use std::collections::BTreeSet;

use resonantdust_content::recipe_core::{
    recipe, Duration, Entity, RecipeDef, RecipeType, StackDirection,
};
use spacetimedb::{reducer, table, ReducerContext, ScheduleAt, Table, TimeDuration};

use crate::action_completion;
// Bring the generated `cards` table-accessor trait into scope so
// `ctx.db.cards()` resolves (the `cards::latest` module helper is the
// other consumer here).
use crate::cards::{self, cards as _cards_table};
use crate::packed::{
    pack_micro_zone, pack_slot_micro_zone, pack_valid_at, unpack_micro_zone,
    valid_at_time, StackedState, STACK_DIR_DOWN, STACK_DIR_UP,
};

/// `progress_style` bit field (bits 8..=10) packing — mirrors
/// `action_completion::pack_progress_style`. Kept local because that
/// helper is private; promoting it would just move the duplication
/// elsewhere and the bit shift is trivially small.
const PROGRESS_STYLE_SHIFT: u32 = 8;
const PROGRESS_STYLE_MASK: u32 = 0b111 << PROGRESS_STYLE_SHIFT;
fn pack_progress_style(value: u32) -> u32 {
    (value & 0b111) << PROGRESS_STYLE_SHIFT
}
use crate::recipe_eval::entity_specificity;
use resonantdust_content::definition_core::decode_definition;

// Flag bit positions (mirroring `action_completion.rs` / `on_create.rs`).
// Kept local rather than promoted to a shared module — that's a project-
// wide cleanup the codebase hasn't done yet, and copying the few bits we
// need here doesn't justify the refactor on its own.
const FLAG_POSITION_HOLD: u32 = 1 << 0;
const FLAG_DROP_HOLD: u32 = 1 << 3;
const FLAG_SLOT_HOLD: u32 = 1 << 5;
const FLAG_DEAD: u32 = 1 << 7;
const FLAG_FORCE_POSITION: u32 = 1 << 11;
const FLAG_MAGNETIC_HOLD: u32 = 1 << 12;

/// Server-side state for one installed magnetic action. Private — clients
/// observe magnetic state via the anchor card's `magnetic_hold` flag and
/// the leaf cards stacked on it. `magnetic_id` is opaque to clients.
///
/// `scheduled_at` is set to `ScheduleAt::Interval(recipe.interval)` at
/// install — the scheduler then fires `magnetic_tick` every interval
/// until the row is deleted (anchor death) or the action completes.
#[table(accessor = magnetic_actions, scheduled(magnetic_tick), public)]
#[derive(Clone)]
pub struct MagneticAction {
    #[primary_key]
    #[auto_inc]
    pub magnetic_id: u64,
    #[index(btree)]
    pub anchor_card_id: u32,
    pub scheduled_at: ScheduleAt,
    /// Packed id of the outer (on_create.magnetic) recipe that installed
    /// this action. Currently only used for diagnostics; the tick reducer
    /// reads `success_recipe_id` / `failure_recipe_id` directly.
    pub outer_recipe_id: u16,
    pub success_recipe_id: u16,
    pub failure_recipe_id: u16,
    /// Wall-clock second the magnetic phase deadlines at — install time
    /// plus `outer.duration`. Once a tick fires at or past this, the
    /// final-attempt branch runs.
    pub duration_at: u32,
    /// Seconds added between commit decision and inner-recipe start.
    /// Snapshotted from the recipe at install so an in-flight tick
    /// doesn't change behaviour mid-cycle if the value is ever made
    /// per-recipe.
    pub delay_secs: u32,
    /// The player whose inventory we poll for matching cards, and the
    /// fallback owner for products that resolve as `ProductOwner::Action`.
    pub caller_player_id: u32,
    /// Card ids pulled onto the anchor so far, in fill order. Cleared on
    /// each commit (success or failure) — recurring restarts re-use the
    /// same row.
    pub pulled_cards: Vec<u32>,
}

/// Install a magnetic action on `anchor_card_id`.
///
/// Called from [`crate::on_create::trigger`]'s magnetic branch when a
/// freshly-created card matches an `on_create.magnetic` recipe. Applies
/// `recipe.set_start.hex` to the anchor (locking semantics like
/// `drop_locked` / `surface_locked`) and auto-ORs `FLAG_MAGNETIC_HOLD`
/// so the client can render the "magnetic action installed" state.
///
/// Stamps the anchor's holds at `anchor.valid_at_time` via `update_with_at`'s
/// find/delete/insert pattern — the held flags land on the anchor's first
/// visible row, no flagless flicker.
///
/// Inserts a `MagneticAction` row with
/// `scheduled_at = ScheduleAt::Interval(recipe.interval seconds)`; the
/// scheduler fires `magnetic_tick` on that cadence until the row is
/// removed.
pub fn install(
    ctx: &ReducerContext,
    recipe_def: &RecipeDef,
    anchor_card_id: u32,
    caller_player_id: u32,
) -> Result<(), String> {
    let magnetic_refs = recipe_def.magnetic.as_ref().ok_or_else(|| {
        format!(
            "magnetic::install: recipe {} ({}): missing magnetic refs (not a magnetic outer)",
            recipe_def.index, recipe_def.id
        )
    })?;
    let interval_secs = recipe_def.interval.ok_or_else(|| {
        format!(
            "magnetic::install: recipe {} ({}): missing interval",
            recipe_def.index, recipe_def.id
        )
    })?;
    if interval_secs == 0 {
        return Err(format!(
            "magnetic::install: recipe {} ({}): interval must be > 0",
            recipe_def.index, recipe_def.id
        ));
    }
    let duration_secs = match &recipe_def.duration {
        Some(Duration::Fixed(s)) => *s,
        Some(Duration::Conditional { .. }) => {
            return Err(format!(
                "magnetic::install: recipe {} ({}): conditional duration not yet supported \
                 for magnetic outers",
                recipe_def.index, recipe_def.id
            ));
        }
        None => {
            return Err(format!(
                "magnetic::install: recipe {} ({}): missing duration",
                recipe_def.index, recipe_def.id
            ));
        }
    };

    let anchor = cards::latest(ctx, anchor_card_id).ok_or_else(|| {
        format!("magnetic::install: anchor card_id={anchor_card_id} not found")
    })?;
    let installed_secs = valid_at_time(anchor.valid_at);
    let duration_at = installed_secs.saturating_add(duration_secs);

    // Apply `set_start.hex` (FlagOps: set + clear masks) plus auto-OR
    // `FLAG_MAGNETIC_HOLD` onto the anchor at install time. Stamped at
    // `installed_secs` (== anchor.valid_at_time) so the holds land on
    // the anchor's first visible row via `write_at`'s find/delete/insert
    // pattern — no flagless flicker for the client. The auto magnetic
    // hold is OR'd *after* set_start so an author can't accidentally
    // clear it via `set_start.hex.magnetic_hold = false` (not currently
    // a parseable flag anyway, but the policy is explicit).
    let set_start_hex = recipe_def.set_start.hex;
    cards::update_with_at(ctx, anchor_card_id, installed_secs, |c| {
        c.flags = set_start_hex.apply(c.flags) | FLAG_MAGNETIC_HOLD;
    });

    // Lay down a future-stamped row on the anchor at `duration_at` so
    // the client's `scanProgress` finds a target to fill the magnetic
    // phase's progress bar against: `startSecs` resolves to the
    // anchor's install row (current), `endSecs` to this row at
    // `duration_at`. The row clears `FLAG_MAGNETIC_HOLD` (the
    // magnetic phase ends at the deadline — also makes the 🧲
    // indicator disappear once it promotes) and writes the recipe's
    // declared `progress_style` (LTR by default) into the
    // `progress_style` bit field. `commit_success` / `commit_failure`
    // delete this row on early commit so the bar disappears at the
    // commit moment rather than waiting for `duration_at`.
    let progress_style_field = pack_progress_style(recipe_def.style as u32);
    cards::update_with_at(ctx, anchor_card_id, duration_at, |c| {
        c.flags = (c.flags & !FLAG_MAGNETIC_HOLD & !PROGRESS_STYLE_MASK)
            | progress_style_field;
    });

    let interval_micros = (interval_secs as i64).saturating_mul(1_000_000);
    ctx.db.magnetic_actions().insert(MagneticAction {
        magnetic_id: 0,
        anchor_card_id,
        scheduled_at: ScheduleAt::Interval(TimeDuration::from_micros(interval_micros)),
        outer_recipe_id: recipe_def.index,
        success_recipe_id: magnetic_refs.success,
        failure_recipe_id: magnetic_refs.failure,
        duration_at,
        // `delay` is optional in the recipe JSON — `None` collapses to
        // 0 here (the commit decision and the inner recipe's start fall
        // on the same second). Use a positive value when the recipe
        // wants a timing-drift buffer between the release/with-holds
        // rows and the inner's set_start.
        delay_secs: recipe_def.delay.unwrap_or(0),
        caller_player_id,
        pulled_cards: Vec::new(),
    });

    Ok(())
}

/// Tick reducer. Scheduler fires this every `recipe.interval` seconds for
/// each `MagneticAction` row.
///
/// Sequence on each tick:
///
/// 1. **Anchor sanity** — if the anchor row is missing or has
///    `FLAG_DEAD` set, release any pulled cards back to inventory loose
///    and delete this magnetic action. Scheduler stops firing.
/// 2. **Resolve the inner success recipe** — its `slots.len()` is the
///    target fill count; its `recipe_type` (`Magnetic(Up|Down)`) gives
///    the stacking direction.
/// 3. **Final-tick branch** (`now >= duration_at`) — one last fill
///    attempt, then commit success if filled or commit failure
///    otherwise.
/// 4. **Regular-tick branch** — one fill attempt per tick (pulled one
///    card at a time, by design — gives the player time to observe
///    each pull and keeps the cadence predictable). If the fill
///    completes the slot list this tick, commit success early.
///
/// On commit (success or failure) the row is **deleted** rather than
/// reset for a recurring cycle — recurring restart is a follow-up
/// enhancement (design's "Recurring restart" section). The despair
/// recipe consumes the anchor as a reagent anyway, so single-shot
/// covers the canonical case.
#[reducer]
pub fn magnetic_tick(ctx: &ReducerContext, args: MagneticAction) -> Result<(), String> {
    let anchor_alive = cards::latest(ctx, args.anchor_card_id)
        .is_some_and(|c| c.flags & FLAG_DEAD == 0);
    if !anchor_alive {
        // Anchor missing or dying. Release any cards we'd already
        // pulled onto it back to inventory loose, drop the stranded
        // `duration_at` progress row on the anchor (defensive — if
        // the anchor is missing entirely the delete is a no-op; if
        // it's dying the bar would otherwise still be reachable until
        // the row was reaped), then delete this magnetic action so
        // the scheduler stops firing.
        release_pulled_to_inventory(ctx, &args.pulled_cards);
        ctx.db
            .cards()
            .valid_at()
            .delete(pack_valid_at(args.anchor_card_id, args.duration_at));
        ctx.db
            .magnetic_actions()
            .magnetic_id()
            .delete(args.magnetic_id);
        return Ok(());
    }

    let success_recipe = recipe(args.success_recipe_id)
        .map_err(|e| format!("magnetic_tick: success recipe lookup: {e}"))?
        .ok_or_else(|| {
            format!(
                "magnetic_tick: success recipe id={} not registered",
                args.success_recipe_id
            )
        })?;
    let direction = magnetic_direction(success_recipe)?;
    let needed = needed_count(success_recipe);
    let now = now_secs(ctx);

    let mut args = args;

    // Pull every available card this tick — up to `needed` total, or
    // until inventory runs out of matches for the next slot's entity.
    // Previously we pulled one card per tick (predictable cadence,
    // visible speckle), but with server-side instant commit on full
    // fill that cadence wasn't doing much for the player visually.
    // Trying the all-at-once approach to see how it reads.
    //
    // The loop stops as soon as `find_match` returns `None` for the
    // current slot: missing-from-inventory means we *can't* fill any
    // further, and waiting for the next tick is the only recourse
    // (player might drop something in the meantime).
    let initial_pulled_count = args.pulled_cards.len();
    while args.pulled_cards.len() < needed {
        let Some(entity) = entity_at_pull_index(success_recipe, args.pulled_cards.len()) else {
            break;
        };
        let Some(c) = find_match(ctx, &args, entity)? else {
            break;
        };
        magnetize(ctx, c, &args, args.pulled_cards.len(), direction)?;
        args.pulled_cards.push(c);
    }

    // Final-tick branch: at or past the magnetic phase deadline,
    // commit one way or the other. The pull loop above is our "one
    // last fill attempt"; whatever it produced is what we have.
    if now >= args.duration_at {
        if args.pulled_cards.len() == needed {
            commit_success(ctx, &args, success_recipe, now)?;
        } else {
            commit_failure(ctx, &args, now)?;
        }
        return Ok(());
    }

    // Regular tick: if filling completed in this tick, commit success
    // now (skip waiting for `duration_at`). Otherwise persist the new
    // partial-fill count so subsequent ticks pick up from here.
    if args.pulled_cards.len() == needed && needed > 0 {
        commit_success(ctx, &args, success_recipe, now)?;
    } else if args.pulled_cards.len() != initial_pulled_count {
        ctx.db
            .magnetic_actions()
            .magnetic_id()
            .update(args.clone());
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Wall-clock now in unix seconds (u32). Mirrors the same conversion
/// used everywhere else server-side (`cards::now_secs`, `actions.rs`'s
/// `start_secs` derivation).
fn now_secs(ctx: &ReducerContext) -> u32 {
    (ctx.timestamp.to_micros_since_unix_epoch() / 1_000_000) as u32
}

/// Map a magnetic inner recipe's `RecipeType::Magnetic(Up|Down)` to the
/// `STACK_DIR_*` constant used in `pack_stack_micro_zone`. Errors when
/// the success recipe isn't a `Magnetic(_)` — that's a content-side
/// integrity bug and we'd rather fail loud than silently stack the
/// wrong direction.
fn magnetic_direction(recipe: &RecipeDef) -> Result<u8, String> {
    match recipe.recipe_type {
        RecipeType::Magnetic(StackDirection::Up) => Ok(STACK_DIR_UP),
        RecipeType::Magnetic(StackDirection::Down) => Ok(STACK_DIR_DOWN),
        ref other => Err(format!(
            "magnetic_tick: inner success recipe {} ({}) has non-magnetic type {:?}",
            recipe.index, recipe.id, other
        )),
    }
}

/// How many cards a magnetic inner recipe wants pulled. Equals
/// `(1 if root else 0) + slots.len()`. The recipe's `hex` slot is the
/// anchor (already in the chain at install time) and is not counted.
///
/// Note: when `success_recipe.root.is_some()`, the magnetic ticker
/// treats the first pulled card as the recipe's **root** and any
/// subsequent pulls as **slots** in order. This matches the
/// `despair_success` recipe's shape (`root: ["dread"]`, `slots: []`).
/// The design doc's slots-only example is a simpler case of the same
/// rule (`root.is_none()`).
fn needed_count(recipe: &RecipeDef) -> usize {
    (if recipe.root.is_some() { 1 } else { 0 }) + recipe.slots.len()
}

/// Which entity the next pull should match, given the current count of
/// already-pulled cards. Returns `None` once every role is filled.
///
/// Order is `[root, slots[0], slots[1], ...]`. When the recipe has no
/// `root` entity, indexing collapses to plain `slots[pull_index]`.
fn entity_at_pull_index(recipe: &RecipeDef, pull_index: usize) -> Option<&Entity> {
    let root_offset = if recipe.root.is_some() { 1 } else { 0 };
    if root_offset == 1 && pull_index == 0 {
        recipe.root.as_ref()
    } else {
        recipe.slots.get(pull_index - root_offset)
    }
}

/// True if the recipe expects its `root` slot filled from a magnetic
/// pull (i.e. `root.is_some()` and so pulled card #0 is the root, not
/// a slot). Used by `commit_success` to split `pulled_cards` into the
/// `root` / `slots` shape `action_completion::apply` expects.
fn recipe_pulls_root(recipe: &RecipeDef) -> bool {
    recipe.root.is_some()
}

/// Find the best-matching card in the caller's inventory for `entity`.
/// Returns the candidate with the highest `entity_specificity` score;
/// ties broken by lowest `card_id` (deterministic, stable across ticks).
///
/// Filters: card must be owned by `args.caller_player_id`, not the
/// anchor, not already pulled, not flagged with `slot_hold` /
/// `magnetic_hold` / `dead`, and its definition must satisfy the
/// entity with non-zero specificity.
///
/// Walks `ctx.db.cards().iter()` — the cards table doesn't currently
/// btree-index `owner_id` (see the comment on `players::delete_player`),
/// so this is an O(N) scan over all version rows. Dedupes per-card via
/// `cards::latest`. Fine while the table is small; if it becomes hot,
/// add the index and switch to `cards().owner_id().filter(...)`.
fn find_match(
    ctx: &ReducerContext,
    args: &MagneticAction,
    entity: &Entity,
) -> Result<Option<u32>, String> {
    // Enumerate distinct card_ids once, then ask `cards::latest` for
    // each — version-row dedupe happens implicitly via the BTreeSet.
    let mut card_ids: BTreeSet<u32> = BTreeSet::new();
    for c in ctx.db.cards().iter() {
        card_ids.insert(c.card_id);
    }

    let exclude: BTreeSet<u32> = args.pulled_cards.iter().copied().collect();

    let mut best: Option<(u32, u32)> = None; // (card_id, specificity)
    for id in card_ids {
        if id == args.anchor_card_id {
            continue;
        }
        if exclude.contains(&id) {
            continue;
        }
        let card = match cards::latest(ctx, id) {
            Some(c) => c,
            None => continue,
        };
        if card.owner_id != args.caller_player_id {
            continue;
        }
        if card.flags & (FLAG_SLOT_HOLD | FLAG_MAGNETIC_HOLD | FLAG_DEAD) != 0 {
            continue;
        }
        let def = match decode_definition(card.packed_definition) {
            Ok(Some(d)) => d,
            _ => continue,
        };
        let score = entity_specificity(entity, def);
        if score == 0 {
            continue;
        }
        match best {
            None => best = Some((id, score)),
            Some((bid, bscore)) => {
                // Tie-break by lowest card_id so the choice is stable
                // across ticks (otherwise a tie could flip on every
                // tick depending on iteration order).
                if score > bscore || (score == bscore && id < bid) {
                    best = Some((id, score));
                }
            }
        }
    }
    Ok(best.map(|(id, _)| id))
}

/// Magnetize one card into the magnetic chain at position `leaf_index`.
/// Writes are NOW-stamped via `cards::update_with`.
///
/// Spatial layout mirrors `actions.rs::propose_action`'s chain stitch:
///
/// - **First pull (`leaf_index == 0`)** — the chain bottom. Whether the
///   recipe declares `root` or just relies on `slots[0]` (recipe slot
///   1, 1-indexed in `reagents` syntax), this is the card that sits
///   directly on the hex anchor. State = `OnHex`, `micro_location` =
///   `anchor_card_id`, `micro_zone` packs the anchor's local `(q, r)`
///   under `StackedState::OnHex`.
///
/// - **Subsequent pulls (`leaf_index > 0`)** — daisy-chained above the
///   previous pull. State = `Slot`, `micro_location` =
///   `args.pulled_cards[leaf_index - 1]` (parent pointer to the
///   immediately-preceding pull), `micro_zone` =
///   `pack_slot_micro_zone(direction)` (direction bit set; position
///   from root is implicit — walk parent pointers until the chain
///   bottom). Identical encoding to the `slots[1..]` loop in
///   `propose_action`.
///
/// The role split (root vs slot[0]) is handled at commit time by
/// `commit_success`'s `pulls_root`-based partition; this function
/// doesn't need to care because the *spatial* layout is the same in
/// both cases. The previous design ("all pulls are leaves on the
/// anchor") was replaced with this chain layout to make recipes like
/// `strike_success` (root: corpus, slots: [corpus, corpus]) stack
/// visually as `strike → corpus → corpus → corpus` instead of fanning
/// the three corpus out as siblings on the strike hex.
fn magnetize(
    ctx: &ReducerContext,
    card_id: u32,
    args: &MagneticAction,
    leaf_index: usize,
    direction: u8,
) -> Result<(), String> {
    let anchor = cards::latest(ctx, args.anchor_card_id).ok_or_else(|| {
        format!(
            "magnetic_tick: anchor card_id={} disappeared mid-tick",
            args.anchor_card_id
        )
    })?;

    let (new_micro_zone, new_micro_location) = if leaf_index == 0 {
        // Chain bottom — pin to the hex anchor. Decode anchor's local
        // `(q, r)` and repack with `OnHex` state. Same shape
        // `propose_action`'s hex-anchored slots[0] uses.
        let (q, r, _) = unpack_micro_zone(anchor.micro_zone);
        (
            pack_micro_zone(q, r, StackedState::OnHex),
            args.anchor_card_id,
        )
    } else {
        // Above-the-bottom — state=Slot parent-pointer onto previous
        // pull. `pack_slot_micro_zone` writes
        // `[position=0, direction, state=Slot]`; chain position is
        // implicit (walk parent chain until non-Slot state).
        let parent_id = args.pulled_cards[leaf_index - 1];
        (pack_slot_micro_zone(direction), parent_id)
    };

    cards::update_with(ctx, card_id, |c| {
        c.micro_zone = new_micro_zone;
        c.micro_location = new_micro_location;
        c.macro_zone = anchor.macro_zone;
        c.surface = anchor.surface;
        // `SLOT_HOLD` here mirrors `propose_action`'s
        // "every card claimed by an in-flight action carries slot_hold"
        // rule — pulled cards are slot-claimed by the magnetic action
        // for the duration of the magnetic phase + inner recipe, so
        // other action proposals' slot_hold guard rejects them.
        // Released alongside POSITION_HOLD / DROP_HOLD on commit
        // (via `PULL_FLAGS_MASK` on failure / anchor-death paths, and
        // via `action_completion::apply`'s `release_mask` on the
        // success path).
        c.flags |= FLAG_POSITION_HOLD | FLAG_DROP_HOLD | FLAG_SLOT_HOLD | FLAG_FORCE_POSITION;
    });
    Ok(())
}

/// Mask of pull-side flags magnetize sets on pulled cards. Cleared (or
/// partially cleared) on commit.
const PULL_FLAGS_MASK: u32 =
    FLAG_POSITION_HOLD | FLAG_DROP_HOLD | FLAG_SLOT_HOLD | FLAG_FORCE_POSITION;

/// Commit the magnetic action's **success** path:
///
/// 1. For each pulled card, write a **single combined row** at
///    `commit_at = now + delay_secs`: clear `FLAG_FORCE_POSITION` (the
///    chain is now stable, no more force-position semantics needed) and
///    OR in the inner success recipe's `set_start.slot` bits. Keep
///    `FLAG_POSITION_HOLD` / `FLAG_DROP_HOLD` / `FLAG_SLOT_HOLD` — the
///    inner recipe is about to claim the cards anyway and we don't
///    want a flicker between magnetic release and inner set_start.
///    `action_completion::apply` will clear all three holds on the
///    inner recipe's completion row via its `release_mask`.
/// 2. Hand off to `action_completion::apply` for the inner success
///    recipe at `commit_at + inner.duration`, passing the anchor as
///    `hex`, `root = 0`, and the pulled cards as `slots`.
/// 3. Delete this magnetic_action row (single-shot — recurring restart
///    is a future enhancement).
fn commit_success(
    ctx: &ReducerContext,
    args: &MagneticAction,
    success_recipe: &RecipeDef,
    now: u32,
) -> Result<(), String> {
    // No delay on the success path — by the time we get here the
    // server has already written every pull row (visible to the
    // client as the chain assembled) and the conditions to start the
    // inner recipe are met. The previous `now + delay_secs` was a
    // timing-drift buffer carried over from an earlier user-driven
    // claim design; server-driven polling has no such race, so the
    // recipe can start at `now` and the client sees the inner
    // recipe's progress begin at the same moment the last pull
    // landed. `delay_secs` is left on the `MagneticAction` row for
    // now — `commit_failure` still uses it, and removing it is a
    // schema change worth its own pass.
    let commit_at = now;
    let set_start_root = success_recipe.set_start.root;
    let set_start_slot = success_recipe.set_start.slot;
    let pulls_root = recipe_pulls_root(success_recipe);

    // Per pulled card: apply the inner success recipe's set_start
    // FlagOps for that card's role (root vs slot). Preserve
    // POSITION_HOLD / DROP_HOLD / SLOT_HOLD / FORCE_POSITION on the
    // same write so the inner recipe's `set_start` lands on a row
    // that already carries them — no flagless flicker between
    // magnetic release and inner set_start.
    //
    // FORCE_POSITION specifically must remain set through `commit_at`:
    // with no `delay_secs` between magnetize and commit, magnetize and
    // commit write rows at the same `valid_at_time`, and clearing it
    // here would mean the client never sees a row with force_position
    // set — its mirror would preserve the pre-magnetize local position
    // instead of trusting the chain bytes the server wrote. The
    // consumption path in `action_completion::apply` clears
    // FORCE_POSITION (and the other holds) at `completion_secs` via
    // `release_mask`, so cleanup still lands at the natural moment.
    //
    // Author's set_start runs *last* so explicit `slot_hold: false`
    // can override the magnetic-pull slot_hold here.
    for (i, &card_id) in args.pulled_cards.iter().enumerate() {
        let role_ops = if pulls_root && i == 0 {
            set_start_root
        } else {
            set_start_slot
        };
        cards::update_with_at(ctx, card_id, commit_at, |c| {
            c.flags = role_ops.apply(c.flags);
        });
    }

    // Split pulled_cards into (root, slots) so `action_completion::apply`
    // sees the shape the recipe author declared in JSON. When
    // `pulls_root` is false the whole list is slots and root passes as 0.
    let (root_id, slots_for_apply): (u32, &[u32]) = if pulls_root && !args.pulled_cards.is_empty() {
        (args.pulled_cards[0], &args.pulled_cards[1..])
    } else {
        (0, &args.pulled_cards[..])
    };

    // End the magnetic phase before handing off to action_completion:
    // delete the future progress-target row on the anchor (bar
    // disappears at commit) and clear `FLAG_MAGNETIC_HOLD` (🧲
    // indicator disappears). Done before `apply` so the FLAG_DEAD
    // row action_completion writes for the anchor (when it's a
    // reagent) inherits the already-cleared magnetic flag.
    end_magnetic_phase(ctx, args.anchor_card_id, args.duration_at);

    let inner_duration = inner_duration_secs(success_recipe)?;
    let completion_secs = commit_at.saturating_add(inner_duration);
    action_completion::apply(
        ctx,
        success_recipe,
        /* hex          */ args.anchor_card_id,
        /* root         */ root_id,
        /* slots        */ slots_for_apply,
        completion_secs,
        args.caller_player_id,
        /* hex_location */ None,
    )?;

    ctx.db
        .magnetic_actions()
        .magnetic_id()
        .delete(args.magnetic_id);
    Ok(())
}

/// Commit the magnetic action's **failure** path:
///
/// 1. For each pulled card, write a release row at `commit_at` clearing
///    every pull-side flag (`POSITION_HOLD | DROP_HOLD | FORCE_POSITION |
///    MAGNETIC_HOLD` — the last is defensive; pulled cards don't carry
///    it). Spatial fields untouched: the cards stay as leaves on the
///    anchor visually until the failure recipe runs (the failure recipe
///    only acts on the anchor; pulled cards aren't slot-claimed by it).
/// 2. Hand off to `action_completion::apply` for the failure recipe
///    with `slots = []` — failure runs only against the anchor.
/// 3. Delete this magnetic_action row.
fn commit_failure(
    ctx: &ReducerContext,
    args: &MagneticAction,
    now: u32,
) -> Result<(), String> {
    let commit_at = now.saturating_add(args.delay_secs);

    let failure_recipe = recipe(args.failure_recipe_id)
        .map_err(|e| format!("magnetic_tick: failure recipe lookup: {e}"))?
        .ok_or_else(|| {
            format!(
                "magnetic_tick: failure recipe id={} not registered",
                args.failure_recipe_id
            )
        })?;

    let release_mask = !(PULL_FLAGS_MASK | FLAG_MAGNETIC_HOLD);
    for &card_id in &args.pulled_cards {
        cards::update_with_at(ctx, card_id, commit_at, |c| {
            c.flags &= release_mask;
        });
    }

    // Same magnetic-phase teardown as `commit_success`.
    end_magnetic_phase(ctx, args.anchor_card_id, args.duration_at);

    let inner_duration = inner_duration_secs(failure_recipe)?;
    let completion_secs = commit_at.saturating_add(inner_duration);
    action_completion::apply(
        ctx,
        failure_recipe,
        /* hex          */ args.anchor_card_id,
        /* root         */ 0,
        /* slots        */ &[],
        completion_secs,
        args.caller_player_id,
        /* hex_location */ None,
    )?;

    ctx.db
        .magnetic_actions()
        .magnetic_id()
        .delete(args.magnetic_id);
    Ok(())
}

/// End the magnetic phase visually on the anchor: delete the future
/// `duration_at` row (so the client's progress bar's target vanishes
/// and the bar with it) and clear `FLAG_MAGNETIC_HOLD` on the anchor's
/// latest row (so the client's 🧲 indicator disappears). Called from
/// both `commit_success` and `commit_failure` — the magnetic phase
/// ends in either branch, only the inner recipe diverges.
///
/// Idempotent: deleting a non-existent row is a no-op, and clearing
/// an already-clear flag is a no-op. Safe to call even when no
/// `duration_at` row was ever written (e.g. install was somehow
/// skipped).
fn end_magnetic_phase(ctx: &ReducerContext, anchor_id: u32, duration_at: u32) {
    let duration_at_key = pack_valid_at(anchor_id, duration_at);
    ctx.db.cards().valid_at().delete(duration_at_key);
    cards::update_with(ctx, anchor_id, |c| {
        c.flags &= !FLAG_MAGNETIC_HOLD;
    });
}

/// Anchor-died teardown: best-effort release every card that had been
/// pulled onto the anchor back to inventory-loose (clear pull flags +
/// reset spatial). Called only from the anchor-missing/dead branch of
/// `magnetic_tick` where the magnetic action is being torn down before
/// it could commit.
fn release_pulled_to_inventory(ctx: &ReducerContext, pulled_cards: &[u32]) {
    let release_mask = !(PULL_FLAGS_MASK | FLAG_MAGNETIC_HOLD);
    for &card_id in pulled_cards {
        cards::update_with(ctx, card_id, |c| {
            c.flags &= release_mask;
            // Reset to inventory-loose: macro_zone = ownerId,
            // surface = 1, micro_zone = state-0 LOOSE, micro_location
            // = 0 (client's mirror loose-preserve gate will keep
            // whatever local xy the player sees — this server reset
            // is mostly to clear the OnRoot chain reference to a
            // dying anchor).
            c.macro_zone = c.owner_id;
            c.surface = 1;
            c.micro_zone = 0;
            c.micro_location = 0;
        });
    }
}

/// Extract a magnetic inner recipe's `duration` as a fixed second count.
/// Magnetic inner recipes are required to declare `duration` (parser
/// enforces it for non-magnetic-outer recipes), and conditional
/// durations aren't supported here yet — emit a loud error rather than
/// silently fall through.
fn inner_duration_secs(recipe: &RecipeDef) -> Result<u32, String> {
    match &recipe.duration {
        Some(Duration::Fixed(s)) => Ok(*s),
        Some(Duration::Conditional { .. }) => Err(format!(
            "magnetic_tick: inner recipe {} ({}) has conditional duration; not yet supported",
            recipe.index, recipe.id
        )),
        None => Err(format!(
            "magnetic_tick: inner recipe {} ({}) missing required duration",
            recipe.index, recipe.id
        )),
    }
}
