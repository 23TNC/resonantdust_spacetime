// packing.rs

// ---- Zone Packing (u32) ----
// Layout:
// [ zone_q: i12 ][ zone_r: i12 ][ z: u8 ]

pub fn pack_zone(zone_q: i16, zone_r: i16, z: u16) -> u32 {
  (((zone_q as i32 & 0x0FFF) as u32) << 20)
    | (((zone_r as i32 & 0x0FFF) as u32) << 8)
    | (z as u32)
}

pub fn unpack_zone(zone: u32) -> (i16, i16, u16) {
  let zone_q = ((zone >> 20) & 0x0FFF) as i16;
  let zone_r = ((zone >> 8) & 0x0FFF) as i16;
  let z = (zone & 0xFF) as u16;

  // sign extend i12
  let zone_q = if zone_q & 0x0800 != 0 { zone_q | !0x0FFF } else { zone_q };
  let zone_r = if zone_r & 0x0800 != 0 { zone_r | !0x0FFF } else { zone_r };

  (zone_q, zone_r, z)
}

// ---- Position Packing (u8) ----
// Layout:
// [ q: 3 bits ][ r: 3 bits ][ 2 bits reserved ]

pub fn pack_position(q: u8, r: u8) -> u8 {
  ((q & 0x07) << 3) | (r & 0x07)
}

pub fn unpack_position(pos: u8) -> (u8, u8) {
  let q = (pos >> 3) & 0x07;
  let r = pos & 0x07;
  (q, r)
}

// ---- World <-> Zone Helpers ----

pub fn world_to_zone(q: i32, r: i32) -> (i16, i16) {
  (
    (q.div_euclid(8)) as i16,
    (r.div_euclid(8)) as i16,
  )
}

pub fn world_to_position(q: i32, r: i32) -> (u8, u8) {
  (
    q.rem_euclid(8) as u8,
    r.rem_euclid(8) as u8,
  )
}

pub fn zone_to_world(zone_q: i16, zone_r: i16, q: u8, r: u8) -> (i32, i32) {
  (
    zone_q as i32 * 8 + q as i32,
    zone_r as i32 * 8 + r as i32,
  )
}