#![cfg_attr(not(feature = "std"), no_std)]

use core::cmp::Ordering;
#[cfg(feature = "std")]
#[allow(unused)]
use std::time::{Duration, Instant};

#[cfg(feature = "embassy")]
#[allow(unused)]
use embassy_time::{Duration, Instant};

use core::array;
use core::marker::PhantomData;

use serde::de;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use trickle::TrickleParams;

static CRC: crc::Crc<u32> = crc::Crc::<u32>::new(&crc::CRC_32_BZIP2);

pub const TRICKLE_PARAMS: TrickleParams = TrickleParams {
    i_min_micros: 10_000,
    i_max_micros: 10_000_000,
    k: 1,
};

pub const MAX_PACKET_LEN: usize = 300;

#[derive(Clone, Copy, Debug, PartialEq, Eq, defmt::Format)]
pub struct MergeResult {
    pub older: bool,
    pub newer: bool,
}

impl MergeResult {
    pub const CONSISTENT: MergeResult = MergeResult {
        older: false,
        newer: false,
    };
    pub const NEWER: MergeResult = MergeResult {
        older: false,
        newer: true,
    };
    pub const OLDER: MergeResult = MergeResult {
        older: false,
        newer: true,
    };
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, defmt::Format)]
pub struct CommState {
    pub seq_num: u64,
    pub type_: CommType,
}

/*
impl TrickleOrd for CommState {
    fn consider(&self, other: &Self) -> trickle::TrickleOrdering {
        let consider_seq_num = TrickleOrdering::from(other.seq_num.cmp(&self.seq_num));
        consider_seq_num.then_with(|| self.type_.consider(&other.type_))
    }
}
*/

impl CommState {
    pub fn update(&mut self, now: Instant) {
        self.type_.update(now);
    }

    pub fn merge(&mut self, other: &Self) -> MergeResult {
        if other.seq_num > self.seq_num {
            *self = other.clone();
            return MergeResult::NEWER;
        }
        if other.seq_num < self.seq_num {
            return MergeResult::OLDER;
        }
        // seq_num is equal
        return self.type_.merge(&other.type_);
    }

    pub fn propagate(&self) -> [Self; 4] {
        self.type_.propagate().map(|type_| CommState {
            seq_num: self.seq_num,
            type_,
        })
    }

    pub fn try_deserialize_packet(s: &mut [u8]) -> postcard::Result<Self> {
        let sz = cobs::decode_in_place(s).map_err(|_| postcard::Error::DeserializeBadEncoding)?;

        // We can't use postcard's CRC flavor because of our custom "Unknown" deserialization.
        // Postcard's CRC is calculated on the consumed bytes--
        // if the whole packet is not consumed during deserialization
        // (e.g. an unknown variant with data), then the CRC will be wrong.
        // (I looked into convincing it to consume all the bytes but it seemed difficult.)

        // Ensure we have at least 4 bytes for CRC
        if sz < 4 {
            return Err(postcard::Error::DeserializeUnexpectedEnd);
        }

        // Split data and CRC (last 4 bytes)
        let (data, crc_bytes) = s[..sz].split_at(sz - 4);

        // Verify CRC on the entire packet data
        let expected_crc =
            u32::from_le_bytes([crc_bytes[0], crc_bytes[1], crc_bytes[2], crc_bytes[3]]);
        let mut digest = CRC.digest();
        digest.update(data);
        let calculated_crc = digest.finalize();

        if calculated_crc != expected_crc {
            return Err(postcard::Error::DeserializeBadCrc);
        }

        // Deserialize the data without CRC
        postcard::from_bytes(data)
    }

    pub fn serialize_packet<'p>(&self, s: &'p mut [u8]) -> &'p mut [u8] {
        postcard::serialize_with_flavor(
            self,
            postcard::ser_flavors::crc::CrcModifier::new(
                postcard::ser_flavors::Cobs::try_new(postcard::ser_flavors::Slice::new(s)).unwrap(),
                CRC.digest(),
            ),
        )
        .unwrap()
    }
}

pub const COMM_TYPE_INIT: u8 = 0x00;
pub const COMM_TYPE_UNKNOWN: u8 = 0xEF;
pub const COMM_TYPE_BL_INIT: u8 = 0xF0;
pub const COMM_TYPE_BL_BROADCAST_PING: u8 = 0xF1;
pub const COMM_TYPE_BL_CODE_WRITE: u8 = 0xF2;
pub const COMM_TYPE_BL_CODE_PROGRESS: u8 = 0xF3;
pub const COMM_TYPE_BL_INDICATE_GOOD: u8 = 0xF4;
pub const COMM_TYPE_BL_UNKNOWN: u8 = 0xFF;

#[cfg(test)]
pub const COMM_TYPE_TEST_APP_UNKNOWN: u8 = 0xEE;
#[cfg(test)]
pub const COMM_TYPE_TEST_BL_UNKNOWN: u8 = 0xFE;

pub const COMM_TYPE_BL_BITMASK: u8 = COMM_TYPE_BL_INIT;

#[derive(Debug, Clone, defmt::Format)]
#[repr(u8)]
pub enum CommType {
    #[cfg(feature = "app")]
    Init = COMM_TYPE_INIT,
    Unknown = COMM_TYPE_UNKNOWN,
    #[cfg(feature = "bl")]
    BlInit = COMM_TYPE_BL_INIT,
    #[cfg(feature = "bl")]
    BlBroadcastPing(BlBroadcastPing) = COMM_TYPE_BL_BROADCAST_PING,
    #[cfg(feature = "bl")]
    BlCodeWrite(BlCodeWrite) = COMM_TYPE_BL_CODE_WRITE,
    #[cfg(feature = "bl")]
    BlCodeProgress(BlCodeProgress) = COMM_TYPE_BL_CODE_PROGRESS,
    #[cfg(feature = "bl")]
    BlIndicateGood = COMM_TYPE_BL_INDICATE_GOOD,
    BlUnknown = COMM_TYPE_BL_UNKNOWN,
    #[cfg(test)]
    TestAppUnknown(u32) = COMM_TYPE_TEST_APP_UNKNOWN,
    #[cfg(test)]
    TestBlUnknown(u32) = COMM_TYPE_TEST_BL_UNKNOWN,
}

impl Serialize for CommType {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match *self {
            #[cfg(feature = "app")]
            CommType::Init => Serializer::serialize_unit_variant(
                serializer,
                "CommType",
                COMM_TYPE_INIT as u32,
                "Init",
            ),
            CommType::Unknown => Serializer::serialize_unit_variant(
                serializer,
                "CommType",
                COMM_TYPE_UNKNOWN as u32,
                "Unknown",
            ),
            #[cfg(feature = "bl")]
            CommType::BlInit => Serializer::serialize_unit_variant(
                serializer,
                "CommType",
                COMM_TYPE_BL_INIT as u32,
                "BlInit",
            ),
            #[cfg(feature = "bl")]
            CommType::BlBroadcastPing(ref data) => Serializer::serialize_newtype_variant(
                serializer,
                "CommType",
                COMM_TYPE_BL_BROADCAST_PING as u32,
                "BlBroadcastPing",
                data,
            ),
            #[cfg(feature = "bl")]
            CommType::BlCodeWrite(ref data) => Serializer::serialize_newtype_variant(
                serializer,
                "CommType",
                COMM_TYPE_BL_CODE_WRITE as u32,
                "BlCodeWrite",
                data,
            ),
            #[cfg(feature = "bl")]
            CommType::BlCodeProgress(ref data) => Serializer::serialize_newtype_variant(
                serializer,
                "CommType",
                COMM_TYPE_BL_CODE_PROGRESS as u32,
                "BlCodeProgress",
                data,
            ),
            #[cfg(feature = "bl")]
            CommType::BlIndicateGood => Serializer::serialize_unit_variant(
                serializer,
                "CommType",
                COMM_TYPE_BL_INDICATE_GOOD as u32,
                "BlIndicateGood",
            ),
            CommType::BlUnknown => Serializer::serialize_unit_variant(
                serializer,
                "CommType",
                COMM_TYPE_BL_UNKNOWN as u32,
                "BlUnknown",
            ),
            #[cfg(test)]
            CommType::TestAppUnknown(ref data) => Serializer::serialize_newtype_variant(
                serializer,
                "CommType",
                COMM_TYPE_TEST_APP_UNKNOWN as u32,
                "TestAppUnknown",
                data,
            ),
            #[cfg(test)]
            CommType::TestBlUnknown(ref data) => Serializer::serialize_newtype_variant(
                serializer,
                "CommType",
                COMM_TYPE_TEST_BL_UNKNOWN as u32,
                "TestBlUnknown",
                data,
            ),
        }
    }
}

impl<'de> Deserialize<'de> for CommType {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct Visitor<'de> {
            marker: PhantomData<CommType>,
            lifetime: PhantomData<&'de ()>,
        }

        impl<'de> de::Visitor<'de> for Visitor<'de> {
            type Value = CommType;
            fn expecting(&self, formatter: &mut core::fmt::Formatter) -> core::fmt::Result {
                core::fmt::Formatter::write_str(formatter, "enum CommType")
            }
            fn visit_enum<A>(self, data: A) -> Result<Self::Value, A::Error>
            where
                A: de::EnumAccess<'de>,
            {
                match de::EnumAccess::variant(data) {
                    #[cfg(feature = "app")]
                    Ok((COMM_TYPE_INIT, variant)) => {
                        de::VariantAccess::unit_variant(variant)?;
                        Ok(CommType::Init)
                    }
                    #[cfg(feature = "bl")]
                    Ok((COMM_TYPE_BL_INIT, variant)) => {
                        de::VariantAccess::unit_variant(variant)?;
                        Ok(CommType::BlInit)
                    }
                    #[cfg(feature = "bl")]
                    Ok((COMM_TYPE_BL_BROADCAST_PING, variant)) => Result::map(
                        de::VariantAccess::newtype_variant::<BlBroadcastPing>(variant),
                        CommType::BlBroadcastPing,
                    ),
                    #[cfg(feature = "bl")]
                    Ok((COMM_TYPE_BL_CODE_WRITE, variant)) => Result::map(
                        de::VariantAccess::newtype_variant::<BlCodeWrite>(variant),
                        CommType::BlCodeWrite,
                    ),
                    #[cfg(feature = "bl")]
                    Ok((COMM_TYPE_BL_CODE_PROGRESS, variant)) => Result::map(
                        de::VariantAccess::newtype_variant::<BlCodeProgress>(variant),
                        CommType::BlCodeProgress,
                    ),
                    #[cfg(feature = "bl")]
                    Ok((COMM_TYPE_BL_INDICATE_GOOD, variant)) => {
                        de::VariantAccess::unit_variant(variant)?;
                        Ok(CommType::BlIndicateGood)
                    }
                    Ok((d, variant)) if (d & COMM_TYPE_BL_BITMASK) == COMM_TYPE_BL_BITMASK => {
                        de::VariantAccess::unit_variant(variant)?;
                        Ok(CommType::BlUnknown)
                    }
                    Ok((_, variant)) => {
                        de::VariantAccess::unit_variant(variant)?;
                        Ok(CommType::Unknown)
                    }
                    Err(err) => Err(err),
                }
            }
        }
        #[doc(hidden)]
        const VARIANTS: &'static [&'static str] = &[
            #[cfg(feature = "app")]
            "Init",
            "Unknown",
            #[cfg(feature = "bl")]
            "BlInit",
            #[cfg(feature = "bl")]
            "BlBroadcastPing",
            #[cfg(feature = "bl")]
            "BlCodeWrite",
            #[cfg(feature = "bl")]
            "BlCodeProgress",
            "BlUnknown",
        ];
        Deserializer::deserialize_enum(
            deserializer,
            "CommType",
            VARIANTS,
            Visitor {
                marker: PhantomData::<CommType>,
                lifetime: PhantomData,
            },
        )
    }
}

impl Default for CommType {
    fn default() -> Self {
        #[cfg(feature = "app")]
        {
            Self::Init
        }
        #[cfg(not(feature = "app"))]
        {
            Self::BlInit
        }
    }
}

impl CommType {
    pub fn discriminant(&self) -> u8 {
        // SAFETY: `Self` is marked `repr(u8)`
        unsafe { *<*const _>::from(self).cast::<u8>() }
    }

    pub fn is_bl(&self) -> bool {
        (self.discriminant() & COMM_TYPE_BL_BITMASK) == COMM_TYPE_BL_BITMASK
    }

    pub fn update(&mut self, now: Instant) {
        match self {
            #[cfg(feature = "bl")]
            CommType::BlBroadcastPing(data) => {
                data.update(now);
            }
            _ => {}
        }
    }

    pub fn propagate(&self) -> [Self; 4] {
        match self {
            #[cfg(feature = "app")]
            CommType::Init => array::from_fn(|_| Self::Init),
            CommType::Unknown => array::from_fn(|_| Self::Unknown),
            #[cfg(feature = "bl")]
            CommType::BlInit => array::from_fn(|_| Self::BlInit),
            #[cfg(feature = "bl")]
            CommType::BlBroadcastPing(data) => {
                array::from_fn(|_| Self::BlBroadcastPing(data.clone()))
            }
            #[cfg(feature = "bl")]
            CommType::BlCodeWrite(data) => array::from_fn(|_| Self::BlCodeWrite(data.clone())),
            #[cfg(feature = "bl")]
            CommType::BlCodeProgress(data) => {
                array::from_fn(|_| Self::BlCodeProgress(data.clone()))
            }
            #[cfg(feature = "bl")]
            CommType::BlIndicateGood => array::from_fn(|_| Self::BlIndicateGood),
            CommType::BlUnknown => array::from_fn(|_| Self::BlUnknown),
            #[cfg(test)]
            CommType::TestAppUnknown(data) => array::from_fn(|_| Self::TestAppUnknown(*data)),
            #[cfg(test)]
            CommType::TestBlUnknown(data) => array::from_fn(|_| Self::TestBlUnknown(*data)),
        }
    }

    pub fn merge(&mut self, other: &Self) -> MergeResult {
        match (&mut *self, other) {
            #[cfg(feature = "app")]
            (CommType::Init, CommType::Init) => MergeResult::CONSISTENT,
            (CommType::Unknown, CommType::Unknown) => MergeResult::CONSISTENT,
            #[cfg(feature = "bl")]
            (CommType::BlInit, CommType::BlInit) => MergeResult::CONSISTENT,
            #[cfg(feature = "bl")]
            (CommType::BlBroadcastPing(s), CommType::BlBroadcastPing(o)) => s.merge(o),
            #[cfg(feature = "bl")]
            (CommType::BlCodeWrite(s), CommType::BlCodeWrite(o)) => s.merge(o),
            #[cfg(feature = "bl")]
            (CommType::BlCodeProgress(s), CommType::BlCodeProgress(o)) => s.merge(o),
            #[cfg(feature = "bl")]
            (CommType::BlIndicateGood, CommType::BlIndicateGood) => MergeResult::CONSISTENT,
            (CommType::BlUnknown, CommType::BlUnknown) => MergeResult::CONSISTENT,
            #[cfg(test)]
            (CommType::TestAppUnknown(_), CommType::TestAppUnknown(_)) => MergeResult::CONSISTENT,
            #[cfg(test)]
            (CommType::TestBlUnknown(_), CommType::TestBlUnknown(_)) => MergeResult::CONSISTENT,
            (s, CommType::Unknown) if !s.is_bl() => MergeResult::CONSISTENT,
            (CommType::Unknown, o) if !o.is_bl() => {
                *self = o.clone();
                MergeResult::NEWER
            }
            (s, CommType::BlUnknown) if s.is_bl() => MergeResult::CONSISTENT,
            (CommType::BlUnknown, o) if o.is_bl() => {
                *self = o.clone();
                MergeResult::NEWER
            }
            (s, o) => {
                // First compare by domain: app types (false) are Greater than BL types (true)
                match s
                    .is_bl()
                    .cmp(&o.is_bl())
                    // Then within same domain: higher discriminant is Greater
                    .then(o.discriminant().cmp(&s.discriminant()))
                {
                    Ordering::Greater => {
                        *self = other.clone();
                        MergeResult::NEWER
                    }
                    Ordering::Equal => MergeResult::CONSISTENT,
                    Ordering::Less => MergeResult::OLDER,
                }
            }
        }
    }

    /*
    pub fn consider(&self, other: &Self) -> TrickleOrdering {
        match (self, other) {
            #[cfg(feature = "app")]
            (CommType::Init, CommType::Init) => TrickleOrdering::Consistent,
            (CommType::Unknown, CommType::Unknown) => TrickleOrdering::Consistent,
            #[cfg(feature = "bl")]
            (CommType::BlInit, CommType::BlInit) => TrickleOrdering::Consistent,
            #[cfg(feature = "bl")]
            (CommType::BlBroadcastPing(s), CommType::BlBroadcastPing(o)) => s.consider(o),
            #[cfg(feature = "bl")]
            (CommType::BlCodeWrite(s), CommType::BlCodeWrite(o)) => s.consider(o),
            #[cfg(feature = "bl")]
            (CommType::BlCodeProgress(s), CommType::BlCodeProgress(o)) => s.consider(o),
            #[cfg(feature = "bl")]
            (CommType::BlIndicateGood, CommType::BlIndicateGood) => TrickleOrdering::Consistent,
            (CommType::BlUnknown, CommType::BlUnknown) => TrickleOrdering::Consistent,
            #[cfg(test)]
            (CommType::TestAppUnknown(_), CommType::TestAppUnknown(_)) => {
                TrickleOrdering::Consistent
            }
            #[cfg(test)]
            (CommType::TestBlUnknown(_), CommType::TestBlUnknown(_)) => TrickleOrdering::Consistent,
            (s, CommType::Unknown) if !s.is_bl() => TrickleOrdering::Consistent,
            (CommType::Unknown, o) if !o.is_bl() => TrickleOrdering::Greater,
            (s, CommType::BlUnknown) if s.is_bl() => TrickleOrdering::Consistent,
            (CommType::BlUnknown, o) if o.is_bl() => TrickleOrdering::Greater,
            (s, o) => {
                // First compare by domain: app types (false) are Greater than BL types (true)
                s.is_bl()
                    .cmp(&o.is_bl())
                    // Then within same domain: higher discriminant is Greater
                    .then(o.discriminant().cmp(&s.discriminant()))
                    .into()
            }
        }
    }
    */
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct AgeMicros {
    pub age_micros: u64,
    #[serde(skip)]
    pub last_update: Option<Instant>,
}

impl defmt::Format for AgeMicros {
    fn format(&self, fmt: defmt::Formatter) {
        defmt::write!(fmt, "AgeMicros {{ age_micros: {} }}", self.age_micros)
    }
}

#[cfg(feature = "bl")]
impl AgeMicros {
    /// Update the age_micros field so it is accurate for the given current time
    pub fn update(&mut self, now: Instant) {
        if let Some(last_update) = &mut self.last_update {
            let elapsed_micros = now.duration_since(*last_update).as_micros() as u64;
            self.age_micros += elapsed_micros;
            // Reconstruct the last_update timestamp from the rounded elapsed_micros value
            // so we don't accumulate error
            *last_update += Duration::from_micros(elapsed_micros);
        } else {
            // First update--assume this is done quickly after we receive the age
            self.last_update = Some(now);
        }
    }
}

#[cfg(feature = "bl")]
#[derive(Serialize, Deserialize, Debug, Clone, Default, defmt::Format)]
pub struct BlBroadcastPing {
    pub latency_micros: u64,
    pub age_micros: AgeMicros,
    pub data: heapless::Vec<u8, 256>,
}

impl BlBroadcastPing {
    pub fn update(&mut self, now: Instant) {
        self.age_micros.update(now);
    }

    pub fn merge(&mut self, other: &Self) -> MergeResult {
        // All fields except latency_micros are ignored
        match other.latency_micros.cmp(&self.latency_micros) {
            Ordering::Greater => {
                *self = other.clone();
                MergeResult::NEWER
            }
            Ordering::Equal => MergeResult::CONSISTENT,
            Ordering::Less => MergeResult::OLDER,
        }
    }
}

#[cfg(feature = "bl")]
#[derive(Serialize, Deserialize, Debug, Clone, defmt::Format)]
pub struct BlCodeWrite {
    pub hardware_id: u32,
    pub firmware_size_bytes: u32,
    pub firmware_crc32: u32,
    pub chunk_index: u32,
    pub chunk_data: heapless::Vec<u8, 256>,
}

#[cfg(feature = "bl")]
impl BlCodeWrite {
    pub fn merge(&mut self, other: &Self) -> MergeResult {
        match other.hardware_id.cmp(&self.hardware_id).then(
            other
                .firmware_size_bytes
                .cmp(&self.firmware_size_bytes)
                .then(other.firmware_crc32.cmp(&self.firmware_crc32))
                .then(other.chunk_index.cmp(&self.chunk_index))
                .then_with(|| other.chunk_data.cmp(&self.chunk_data)),
        ) {
            Ordering::Greater => {
                *self = other.clone();
                MergeResult::NEWER
            }
            Ordering::Equal => MergeResult::CONSISTENT,
            Ordering::Less => MergeResult::OLDER,
        }
    }
}

#[cfg(feature = "bl")]
#[derive(Serialize, Deserialize, Debug, Clone, defmt::Format)]
pub struct BlCodeProgress {
    pub hardware_id: u32,
    pub chunk_count: u32,
}

#[cfg(feature = "bl")]
impl BlCodeProgress {
    pub fn merge(&mut self, other: &Self) -> MergeResult {
        // Reverse chunk_count comparison so that lower progress wins
        match other
            .hardware_id
            .cmp(&self.hardware_id)
            .then(other.chunk_count.cmp(&self.chunk_count).reverse())
        {
            Ordering::Greater => {
                *self = other.clone();
                MergeResult::NEWER
            }
            Ordering::Equal => MergeResult::CONSISTENT,
            Ordering::Less => MergeResult::OLDER,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(feature = "app")]
    fn test_init() {
        let state = CommState {
            seq_num: 42,
            type_: CommType::Init,
        };

        let mut buffer = [0u8; MAX_PACKET_LEN];
        let serialized = state.serialize_packet(&mut buffer);
        let deserialized =
            CommState::try_deserialize_packet(serialized).expect("Failed to deserialize packet");

        assert_eq!(state.seq_num, deserialized.seq_num);
        assert!(matches!(deserialized.type_, CommType::Init));
    }

    #[test]
    #[cfg(feature = "bl")]
    fn test_bl_init() {
        let state = CommState {
            seq_num: 999,
            type_: CommType::BlInit,
        };

        let mut buffer = [0u8; MAX_PACKET_LEN];
        let serialized = state.serialize_packet(&mut buffer);
        let deserialized =
            CommState::try_deserialize_packet(serialized).expect("Failed to deserialize packet");

        assert_eq!(state.seq_num, deserialized.seq_num);
        assert!(matches!(deserialized.type_, CommType::BlInit));
    }

    #[test]
    #[cfg(feature = "bl")]
    fn test_bl_broadcast_ping() {
        let state = CommState {
            seq_num: 5678,
            type_: CommType::BlBroadcastPing(BlBroadcastPing {
                latency_micros: 1500,
                age_micros: AgeMicros {
                    age_micros: 3000,
                    last_update: None,
                },
                data: heapless::Vec::from_slice(&[0xAA; 256]).unwrap(),
            }),
        };

        let mut buffer = [0u8; MAX_PACKET_LEN];
        let serialized = state.serialize_packet(&mut buffer);
        let deserialized =
            CommState::try_deserialize_packet(serialized).expect("Failed to deserialize packet");

        assert_eq!(state.seq_num, deserialized.seq_num);
        if let CommType::BlBroadcastPing(ping) = deserialized.type_ {
            assert_eq!(ping.latency_micros, 1500);
            assert_eq!(ping.age_micros.age_micros, 3000);
        } else {
            panic!("Expected BlBroadcastPing variant");
        }
    }

    #[test]
    #[cfg(feature = "bl")]
    fn test_bl_code_write() {
        let mut chunk_data = heapless::Vec::new();
        chunk_data.push(0x01).unwrap();
        chunk_data.push(0x02).unwrap();
        chunk_data.push(0x03).unwrap();
        chunk_data.push(0xFF).unwrap();

        let state = CommState {
            seq_num: 1000,
            type_: CommType::BlCodeWrite(BlCodeWrite {
                hardware_id: 0xDEADBEEF,
                firmware_size_bytes: 25600,
                firmware_crc32: 0x12345678,
                chunk_index: 42,
                chunk_data,
            }),
        };

        let mut buffer = [0u8; MAX_PACKET_LEN];
        let serialized = state.serialize_packet(&mut buffer);
        let deserialized =
            CommState::try_deserialize_packet(serialized).expect("Failed to deserialize packet");

        assert_eq!(state.seq_num, deserialized.seq_num);
        if let CommType::BlCodeWrite(write) = deserialized.type_ {
            assert_eq!(write.hardware_id, 0xDEADBEEF);
            assert_eq!(write.firmware_size_bytes, 25600);
            assert_eq!(write.firmware_crc32, 0x12345678);
            assert_eq!(write.chunk_index, 42);
            assert_eq!(write.chunk_data.len(), 4);
            assert_eq!(write.chunk_data[0], 0x01);
            assert_eq!(write.chunk_data[3], 0xFF);
        } else {
            panic!("Expected BlCodeWrite variant");
        }
    }

    #[test]
    #[cfg(feature = "bl")]
    fn test_bl_code_progress() {
        let state = CommState {
            seq_num: 2048,
            type_: CommType::BlCodeProgress(BlCodeProgress {
                hardware_id: 0x12345678,
                chunk_count: 50,
            }),
        };

        let mut buffer = [0u8; MAX_PACKET_LEN];
        let serialized = state.serialize_packet(&mut buffer);
        let deserialized =
            CommState::try_deserialize_packet(serialized).expect("Failed to deserialize packet");

        assert_eq!(state.seq_num, deserialized.seq_num);
        if let CommType::BlCodeProgress(progress) = deserialized.type_ {
            assert_eq!(progress.hardware_id, 0x12345678);
            assert_eq!(progress.chunk_count, 50);
        } else {
            panic!("Expected BlCodeProgress variant");
        }
    }

    #[test]
    #[cfg(feature = "bl")]
    fn test_packet_with_large_chunk() {
        let mut chunk_data = heapless::Vec::new();
        for i in 0..chunk_data.capacity() {
            chunk_data.push(i as u8).unwrap();
        }

        let state = CommState {
            seq_num: 99999,
            type_: CommType::BlCodeWrite(BlCodeWrite {
                hardware_id: 0xCAFEBABE,
                firmware_size_bytes: 51200,
                firmware_crc32: 0xABCDEF00,
                chunk_index: 150,
                chunk_data,
            }),
        };

        let mut buffer = [0u8; MAX_PACKET_LEN];
        let serialized = state.serialize_packet(&mut buffer);
        let deserialized =
            CommState::try_deserialize_packet(serialized).expect("Failed to deserialize packet");

        assert_eq!(state.seq_num, deserialized.seq_num);
        if let CommType::BlCodeWrite(write) = deserialized.type_ {
            assert_eq!(write.hardware_id, 0xCAFEBABE);
            assert_eq!(write.firmware_size_bytes, 51200);
            assert_eq!(write.firmware_crc32, 0xABCDEF00);
            assert_eq!(write.chunk_index, 150);
            assert_eq!(write.chunk_data.len(), write.chunk_data.capacity());
            for i in 0..write.chunk_data.len() {
                assert_eq!(write.chunk_data[i], i as u8);
            }
        } else {
            panic!("Expected BlCodeWrite variant");
        }
    }

    #[test]
    fn test_unknown() {
        // Create a CommState with TestAppUnknown variant (only available in test)
        let state = CommState {
            seq_num: 5555,
            type_: CommType::TestAppUnknown(0xDEADBEEF),
        };

        let mut buffer = [0u8; MAX_PACKET_LEN];
        let serialized = state.serialize_packet(&mut buffer);
        let deserialized =
            CommState::try_deserialize_packet(serialized).expect("Failed to deserialize packet");

        assert_eq!(state.seq_num, deserialized.seq_num);
        assert!(matches!(deserialized.type_, CommType::Unknown));
    }

    #[test]
    fn test_bl_unknown() {
        // Create a CommState with TestBlUnknown variant (only available in test)
        let state = CommState {
            seq_num: 6666,
            type_: CommType::TestBlUnknown(0xCAFEBABE),
        };

        let mut buffer = [0u8; MAX_PACKET_LEN];
        let serialized = state.serialize_packet(&mut buffer);
        let deserialized =
            CommState::try_deserialize_packet(serialized).expect("Failed to deserialize packet");

        assert_eq!(state.seq_num, deserialized.seq_num);
        assert!(matches!(deserialized.type_, CommType::BlUnknown));
    }

    #[test]
    fn test_corrupted_data_bad_crc() {
        let state = CommState {
            seq_num: 12345,
            type_: CommType::BlInit,
        };

        let mut buffer = [0u8; MAX_PACKET_LEN];
        let serialized = state.serialize_packet(&mut buffer);

        // Make a copy that we can corrupt
        let mut corrupted = [0u8; MAX_PACKET_LEN];
        corrupted[..serialized.len()].copy_from_slice(serialized);

        // Corrupt a byte in the middle of the data (not the CRC itself)
        // COBS encoding puts a 0 delimiter at the end, so corrupt before that
        if corrupted.len() > 5 {
            corrupted[2] ^= 0xFF; // Flip all bits in byte 2
        }

        // Try to deserialize - should fail with bad CRC
        let result = CommState::try_deserialize_packet(&mut corrupted[..serialized.len()]);

        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            postcard::Error::DeserializeBadCrc
        ));
    }

    #[test]
    fn test_compare_unknown_states() {
        // Test app-side states with Unknown
        #[cfg(feature = "app")]
        {
            let init_state = CommState {
                seq_num: 100,
                type_: CommType::Init,
            };
            let unknown_state = CommState {
                seq_num: 100,
                type_: CommType::Unknown,
            };

            assert_eq!(
                init_state.clone().merge(&unknown_state),
                MergeResult::CONSISTENT
            );
            assert_eq!(unknown_state.clone().merge(&init_state), MergeResult::NEWER);
        }

        // Test bootloader-side states with BlUnknown
        #[cfg(feature = "bl")]
        {
            let bl_init_state = CommState {
                seq_num: 200,
                type_: CommType::BlInit,
            };
            let bl_unknown_state = CommState {
                seq_num: 200,
                type_: CommType::BlUnknown,
            };

            assert_eq!(
                bl_init_state.clone().merge(&bl_unknown_state),
                MergeResult::CONSISTENT
            );
            assert_eq!(
                bl_unknown_state.clone().merge(&bl_init_state),
                MergeResult::NEWER
            );

            let bl_ping_state = CommState {
                seq_num: 200,
                type_: CommType::BlBroadcastPing(BlBroadcastPing {
                    latency_micros: 1000,
                    age_micros: AgeMicros {
                        age_micros: 2000,
                        last_update: None,
                    },
                    data: heapless::Vec::new(),
                }),
            };

            assert_eq!(
                bl_ping_state.clone().merge(&bl_unknown_state),
                MergeResult::CONSISTENT
            );
            assert_eq!(
                bl_unknown_state.clone().merge(&bl_ping_state),
                MergeResult::NEWER
            );
        }

        // Test that Unknown and BlUnknown don't cross domains
        let unknown_state = CommState {
            seq_num: 400,
            type_: CommType::Unknown,
        };
        let bl_unknown_state = CommState {
            seq_num: 400,
            type_: CommType::BlUnknown,
        };

        assert!(unknown_state.clone().merge(&bl_unknown_state) != MergeResult::CONSISTENT);
        assert!(bl_unknown_state.clone().merge(&unknown_state) != MergeResult::CONSISTENT);
    }

    #[test]
    fn test_unknown_consistent_with_unknown() {
        // Test that Unknown is consistent with itself
        let unknown_state1 = CommState {
            seq_num: 500,
            type_: CommType::Unknown,
        };
        let unknown_state2 = CommState {
            seq_num: 500,
            type_: CommType::Unknown,
        };

        assert_eq!(
            unknown_state1.clone().merge(&unknown_state2),
            MergeResult::CONSISTENT
        );
        assert_eq!(
            unknown_state2.clone().merge(&unknown_state1),
            MergeResult::CONSISTENT
        );

        // Test that BlUnknown is consistent with itself
        let bl_unknown_state1 = CommState {
            seq_num: 600,
            type_: CommType::BlUnknown,
        };
        let bl_unknown_state2 = CommState {
            seq_num: 600,
            type_: CommType::BlUnknown,
        };

        assert_eq!(
            bl_unknown_state1.clone().merge(&bl_unknown_state2),
            MergeResult::CONSISTENT
        );
        assert_eq!(
            bl_unknown_state2.clone().merge(&bl_unknown_state1),
            MergeResult::CONSISTENT
        );
    }

    #[test]
    #[cfg(all(feature = "app", feature = "bl"))]
    fn test_app_types_greater_than_bl_types() {
        // Test that app types are considered Greater than BL types
        let app_state = CommState {
            seq_num: 0,
            type_: CommType::Init,
        };
        let bl_state = CommState {
            seq_num: 0,
            type_: CommType::BlInit,
        };

        // App.merge(BL) should produce OLDER (app is older than BL)
        assert_eq!(app_state.clone().merge(&bl_state), MergeResult::OLDER);

        // BL.merge(App) should produce NEWER
        // (since App runs after the bootloader
        // and both may have a seq_num of 0)
        assert_eq!(bl_state.clone().merge(&app_state), MergeResult::NEWER);
    }
}
