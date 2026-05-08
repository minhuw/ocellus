const COUNTER_ENABLE_BIT: u32 = 1 << 22;
const COUNTER_OVERFLOW_ENABLE_BIT: u32 = 1 << 20;
const COUNTER_RESET_BIT: u32 = 1 << 17;
const FIXED_COUNTER_ENABLE_BIT: u32 = 1 << 22;
const FIXED_COUNTER_RESET_BIT: u32 = 1 << 19;
const IIO_CHANNEL_MASK_SHIFT: u32 = 36;
const IIO_FUNCTION_CLASS_MASK_SHIFT: u32 = 44;
const UNIT_COUNTER_RESET_BIT: u32 = 1 << 1;
const UNIT_CONTROL_RESET_BIT: u32 = 1 << 0;
const UNIT_FREEZE_BIT: u32 = 1 << 8;
const UNIT_RESERVED_BITS: u32 = 0b11 << 16;

pub const FIXED_COUNTER_RESET_AND_ENABLE: u32 = FIXED_COUNTER_RESET_BIT | FIXED_COUNTER_ENABLE_BIT;
pub const UNIT_FREEZE: u32 = UNIT_FREEZE_BIT | UNIT_RESERVED_BITS;
pub const UNIT_FREEZE_AND_RESET: u32 =
    UNIT_CONTROL_RESET_BIT | UNIT_COUNTER_RESET_BIT | UNIT_FREEZE_BIT | UNIT_RESERVED_BITS;
pub const UNIT_UNFREEZE: u32 = UNIT_RESERVED_BITS;

pub fn counter_control(event: u8, umask: u8, overflow_enabled: bool) -> u32 {
    let overflow = if overflow_enabled {
        COUNTER_OVERFLOW_ENABLE_BIT
    } else {
        0
    };

    u32::from(event) | (u32::from(umask) << 8) | COUNTER_RESET_BIT | overflow | COUNTER_ENABLE_BIT
}

pub fn iio_counter_control(
    event: u8,
    umask: u8,
    channel_mask: u8,
    function_class_mask: u8,
    overflow_enabled: bool,
) -> u64 {
    u64::from(counter_control(event, umask, overflow_enabled))
        | (u64::from(channel_mask) << IIO_CHANNEL_MASK_SHIFT)
        | (u64::from(function_class_mask) << IIO_FUNCTION_CLASS_MASK_SHIFT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_counter_control() {
        assert_eq!(
            counter_control(0x10, 0x20, true),
            0x10 | (0x20 << 8) | (1 << 17) | (1 << 20) | (1 << 22)
        );
    }

    #[test]
    fn encodes_unit_control() {
        assert_eq!(UNIT_FREEZE, 0x30100);
        assert_eq!(UNIT_FREEZE_AND_RESET, 0x30103);
        assert_eq!(UNIT_UNFREEZE, 0x30000);
    }

    #[test]
    fn encodes_iio_counter_control() {
        assert_eq!(
            iio_counter_control(0x41, 0x20, 0xff, 0x07, true),
            u64::from(counter_control(0x41, 0x20, true)) | (0xff_u64 << 36) | (0x07_u64 << 44)
        );
    }
}
