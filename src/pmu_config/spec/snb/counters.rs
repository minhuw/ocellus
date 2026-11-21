//! The general counters each Sandy Bridge-EP and Ivy Bridge-EP uncore event
//! code counts on, as a mask (bit n: counter n), sorted by event code.
//!
//! Source: Intel perfmon (https://github.com/intel/perfmon) revision
//! 7ab0b6e2c09d3df26671c7875148d6ab6e853775, JKT/events/Jaketown_uncore.json
//! (V24, Sandy Bridge-EP; the SNB/ directory holds the client part) and
//! IVT/events/ivytown_uncore.json (V24): the union of the `Counter` fields of
//! all events of a `Unit` and `EventCode`. The guides (327043, 329468) give
//! the same counters for the events they list. Linux's
//! `snbep_uncore_cbox_constraints`, which Ivy Bridge-EP shares, agrees on
//! every event code both list.

/// Sandy Bridge-EP unit CBO.
pub(super) const SNB_CBO: [(u8, u8); 25] = [
    (0x00, 0b1111),
    (0x02, 0b0011),
    (0x03, 0b0011),
    (0x04, 0b0011),
    (0x05, 0b0011),
    (0x06, 0b0011),
    (0x07, 0b0011),
    (0x11, 0b0001),
    (0x12, 0b0011),
    (0x13, 0b0011),
    (0x14, 0b0011),
    (0x1b, 0b1100),
    (0x1c, 0b1100),
    (0x1d, 0b1100),
    (0x1e, 0b1100),
    (0x1f, 0b1110),
    (0x21, 0b0011),
    (0x31, 0b0011),
    (0x32, 0b0011),
    (0x33, 0b0011),
    (0x34, 0b0011),
    (0x35, 0b0011),
    (0x36, 0b0001),
    (0x37, 0b0011),
    (0x39, 0b0011),
];

/// Ivy Bridge-EP unit CBO.
pub(super) const IVB_CBO: [(u8, u8); 25] = [
    (0x00, 0b1111),
    (0x02, 0b0011),
    (0x03, 0b0011),
    (0x04, 0b0011),
    (0x05, 0b0011),
    (0x06, 0b0011),
    (0x07, 0b0011),
    (0x11, 0b0001),
    (0x12, 0b0011),
    (0x13, 0b0011),
    (0x14, 0b0011),
    (0x1b, 0b1100),
    (0x1c, 0b1100),
    (0x1d, 0b1100),
    (0x1e, 0b1100),
    (0x1f, 0b1110),
    (0x31, 0b0011),
    (0x32, 0b0011),
    (0x33, 0b0011),
    (0x34, 0b0011),
    (0x35, 0b0011),
    (0x36, 0b0001),
    (0x37, 0b0011),
    (0x39, 0b0011),
    (0x3c, 0b0011),
];
