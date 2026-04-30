use spacetimedb::ReducerContext;
use crate::cards::insert_panel_card_row;
use crate::packing::CARD_FLAG_STACKABLE;

#[spacetimedb::reducer]
pub fn debug_spawn_vigor(
    ctx: &ReducerContext,
    owner_id: u32,
) -> Result<(), String> {
    insert_panel_card_row(ctx, 2, 0, 5, owner_id, CARD_FLAG_STACKABLE)?;
    Ok(())
}
