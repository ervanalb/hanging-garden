#![cfg_attr(not(feature = "std"), no_std)]

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
use trickle::{TrickleOrd, TrickleOrdering, TrickleParams};

static CRC: crc::Crc<u32> = crc::Crc::<u32>::new(&crc::CRC_32_BZIP2);

pub const TRICKLE_PARAMS: TrickleParams = TrickleParams {
    i_min_micros: 10_000,
    i_max_micros: 10_000_000,
    k: 1,
};

pub const MAX_PACKET_LEN: usize = 300;

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct CommState {
    pub seq_num: u64,
    pub type_: CommType,
}

impl TrickleOrd for CommState {
    fn consider(&self, other: &Self) -> trickle::TrickleOrdering {
        let consider_seq_num = TrickleOrdering::from(other.seq_num.cmp(&self.seq_num));
        consider_seq_num.then_with(|| self.type_.consider(&other.type_))
    }
}

impl CommState {
    pub fn update(&mut self, now: Instant) {
        self.type_.update(now);
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
pub const COMM_TYPE_UNKNOWN: u8 = 0x7F;
pub const COMM_TYPE_BL_INIT: u8 = 0x80;
pub const COMM_TYPE_BL_BROADCAST_PING: u8 = 0x81;
pub const COMM_TYPE_BL_CODE_WRITE: u8 = 0x82;
pub const COMM_TYPE_BL_CODE_PROGRESS: u8 = 0x83;
pub const COMM_TYPE_BL_UNKNOWN: u8 = 0xFF;

#[cfg(test)]
pub const COMM_TYPE_TEST_APP_UNKNOWN: u8 = 0x7E;
#[cfg(test)]
pub const COMM_TYPE_TEST_BL_UNKNOWN: u8 = 0xFE;

pub const COMM_TYPE_BL_BITMASK: u8 = COMM_TYPE_BL_INIT;

#[derive(Debug, Clone)]
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
        /*
        struct FieldVisitor;
        impl<'de> de::Visitor<'de> for FieldVisitor {
            type Value = u8;
            fn expecting(&self, formatter: &mut core::fmt::Formatter) -> core::fmt::Result {
                core::fmt::Formatter::write_str(formatter, "variant identifier")
            }
            fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(value as u8)
            }
            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                match value {
                    #[cfg(feature = "app")]
                    "Init" => Ok(COMM_TYPE_INIT),
                    "Unknown" => Ok(COMM_TYPE_UNKNOWN),
                    #[cfg(feature = "bl")]
                    "BlInit" => Ok(COMM_TYPE_BL_INIT),
                    #[cfg(feature = "bl")]
                    "BlBroadcastPing" => Ok(COMM_TYPE_BL_BROADCAST_PING),
                    #[cfg(feature = "bl")]
                    "BlCodeWrite" => Ok(COMM_TYPE_BL_CODE_WRITE),
                    #[cfg(feature = "bl")]
                    "BlCodeProgress" => Ok(COMM_TYPE_BL_CODE_PROGRESS),
                    "BlUnknown" => Ok(COMM_TYPE_BL_UNKNOWN),
                    _ => Err(de::Error::unknown_variant(value, VARIANTS)),
                }
            }
            fn visit_bytes<__E>(self, __value: &[u8]) -> Result<Self::Value, __E>
            where
                __E: de::Error,
            {
                match __value {
                    #[cfg(feature = "app")]
                    b"Init" => Ok(COMM_TYPE_INIT),
                    b"Unknown" => Ok(COMM_TYPE_UNKNOWN),
                    #[cfg(feature = "bl")]
                    b"BlInit" => Ok(COMM_TYPE_BL_INIT),
                    #[cfg(feature = "bl")]
                    b"BlBroadcastPing" => Ok(COMM_TYPE_BL_BROADCAST_PING),
                    #[cfg(feature = "bl")]
                    b"BlCodeWrite" => Ok(COMM_TYPE_BL_CODE_WRITE),
                    #[cfg(feature = "bl")]
                    b"BlCodeProgress" => Ok(COMM_TYPE_BL_CODE_PROGRESS),
                    b"BlUnknown" => Ok(COMM_TYPE_BL_UNKNOWN),
                    _ => {
                        let __value = &String::from_utf8_lossy(__value);
                        Err(de::Error::unknown_variant(__value, VARIANTS))
                    }
                }
            }
        }
        //#[automatically_derived]
        //impl<'de> Deserialize<'de> for __Field {
        //    #[inline]
        //    fn deserialize<__D>(__deserializer: __D) -> Result<Self, __D::Error>
        //    where
        //        __D: Deserializer<'de>,
        //    {
        //        Deserializer::deserialize_identifier(__deserializer, __FieldVisitor)
        //    }
        //}
        */

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
                    Ok((d, variant)) if (d & COMM_TYPE_BL_BITMASK) == 0 => {
                        de::VariantAccess::unit_variant(variant)?;
                        Ok(CommType::Unknown)
                    }
                    Ok((_, variant)) => {
                        de::VariantAccess::unit_variant(variant)?;
                        Ok(CommType::BlUnknown)
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
        (self.discriminant() & COMM_TYPE_BL_BITMASK) != 0
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
            CommType::BlUnknown => array::from_fn(|_| Self::BlUnknown),
            #[cfg(test)]
            CommType::TestAppUnknown(data) => array::from_fn(|_| Self::TestAppUnknown(*data)),
            #[cfg(test)]
            CommType::TestBlUnknown(data) => array::from_fn(|_| Self::TestBlUnknown(*data)),
        }
    }

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
            (CommType::BlUnknown, CommType::BlUnknown) => TrickleOrdering::Consistent,
            #[cfg(test)]
            (CommType::TestAppUnknown(_), CommType::TestAppUnknown(_)) => {
                TrickleOrdering::Consistent
            }
            #[cfg(test)]
            (CommType::TestBlUnknown(_), CommType::TestBlUnknown(_)) => TrickleOrdering::Consistent,
            #[cfg(feature = "app")]
            (s, CommType::Unknown) if !s.is_bl() => TrickleOrdering::Consistent,
            (s, CommType::BlUnknown) if s.is_bl() => TrickleOrdering::Consistent,
            (s, o) => o.discriminant().cmp(&s.discriminant()).into(),
        }
    }
}

/*
impl Serialize for CommType {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        use serde::ser::SerializeTuple;

        match self {
            #[cfg(feature = "app")]
            CommType::Init => serializer.serialize_u8(COMM_TYPE_INIT),
            CommType::Unknown => serializer.serialize_u8(COMM_TYPE_UNKNOWN),
            #[cfg(feature = "bl")]
            CommType::BlInit => serializer.serialize_u8(COMM_TYPE_BL_INIT),
            #[cfg(feature = "bl")]
            CommType::BlBroadcastPing(data) => {
                let mut tuple = serializer.serialize_tuple(2)?;
                tuple.serialize_element(&COMM_TYPE_BL_BROADCAST_PING)?;
                tuple.serialize_element(data)?;
                tuple.end()
            }
            #[cfg(feature = "bl")]
            CommType::BlCodeWrite(data) => {
                let mut tuple = serializer.serialize_tuple(2)?;
                tuple.serialize_element(&COMM_TYPE_BL_CODE_WRITE)?;
                tuple.serialize_element(data)?;
                tuple.end()
            }
            #[cfg(feature = "bl")]
            CommType::BlCodeProgress(data) => {
                let mut tuple = serializer.serialize_tuple(2)?;
                tuple.serialize_element(&COMM_TYPE_BL_CODE_PROGRESS)?;
                tuple.serialize_element(data)?;
                tuple.end()
            }
            CommType::BlUnknown => serializer.serialize_u8(COMM_TYPE_BL_UNKNOWN),
        }
    }
}

impl<'de> Deserialize<'de> for CommType {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        use serde::de::SeqAccess;

        struct CommTypeVisitor;

        impl<'de> Visitor<'de> for CommTypeVisitor {
            type Value = CommType;

            fn expecting(&self, formatter: &mut core::fmt::Formatter) -> core::fmt::Result {
                formatter.write_str("a CommType discriminant and optional data")
            }

            fn visit_u8<E>(self, discriminant: u8) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                // Unit variants that are just a discriminant byte
                match discriminant {
                    #[cfg(feature = "app")]
                    COMM_TYPE_INIT => Ok(CommType::Init),
                    COMM_TYPE_UNKNOWN => Ok(CommType::Unknown),
                    #[cfg(feature = "bl")]
                    COMM_TYPE_BL_INIT => Ok(CommType::BlInit),
                    COMM_TYPE_BL_UNKNOWN => Ok(CommType::BlUnknown),
                    _ => {
                        // Unknown variant - return appropriate Unknown variant based on bit 0x40
                        if (discriminant & COMM_TYPE_BL_BITMASK) != 0 {
                            Ok(CommType::BlUnknown)
                        } else {
                            Ok(CommType::Unknown)
                        }
                    }
                }
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                // Tuple variants: (discriminant, data)
                let discriminant: u8 = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(0, &self))?;

                match discriminant {
                    #[cfg(feature = "app")]
                    COMM_TYPE_INIT => Ok(CommType::Init),
                    COMM_TYPE_UNKNOWN => Ok(CommType::Unknown),
                    #[cfg(feature = "bl")]
                    COMM_TYPE_BL_INIT => Ok(CommType::BlInit),
                    #[cfg(feature = "bl")]
                    COMM_TYPE_BL_CODE_WRITE => {
                        let data: BlCodeWrite = seq
                            .next_element()?
                            .ok_or_else(|| de::Error::invalid_length(1, &self))?;
                        Ok(CommType::BlCodeWrite(data))
                    }
                    #[cfg(feature = "bl")]
                    COMM_TYPE_BL_CODE_PROGRESS => {
                        let data: BlCodeProgress = seq
                            .next_element()?
                            .ok_or_else(|| de::Error::invalid_length(1, &self))?;
                        Ok(CommType::BlCodeProgress(data))
                    }
                    COMM_TYPE_BL_UNKNOWN => Ok(CommType::BlUnknown),
                    _ => {
                        // Unknown variant - consume any remaining data and return appropriate Unknown variant
                        let _ = seq.next_element::<de::IgnoredAny>();
                        if (discriminant & COMM_TYPE_BL_BITMASK) != 0 {
                            Ok(CommType::BlUnknown)
                        } else {
                            Ok(CommType::Unknown)
                        }
                    }
                }
            }
        }

        deserializer.deserialize_any(CommTypeVisitor)
    }
}
*/

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct AgeMicros {
    pub age_micros: u64,
    #[serde(skip)]
    pub last_update: Option<Instant>,
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
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct BlBroadcastPing {
    pub latency_micros: u64,
    pub age_micros: AgeMicros,
    pub data: heapless::Vec<u8, 256>,
}

#[cfg(feature = "bl")]
impl BlBroadcastPing {
    pub fn update(&mut self, now: Instant) {
        self.age_micros.update(now);
    }

    pub fn consider(&self, other: &Self) -> TrickleOrdering {
        TrickleOrdering::from(other.latency_micros.cmp(&self.latency_micros))
        // age_micros is ignored--always compares equal
    }
}

#[cfg(feature = "bl")]
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct BlCodeWrite {
    pub hardware_id: u32,
    pub chunk_count: u32,
    pub chunk_index: u32,
    pub chunk_data: heapless::Vec<u8, 256>,
}

#[cfg(feature = "bl")]
impl BlCodeWrite {
    pub fn consider(&self, other: &Self) -> TrickleOrdering {
        other.cmp(self).into()
    }
}

#[cfg(feature = "bl")]
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct BlCodeProgress {
    pub hardware_id: u32,
    pub chunk_count: u32,
}

#[cfg(feature = "bl")]
impl BlCodeProgress {
    pub fn consider(&self, other: &Self) -> TrickleOrdering {
        let consider_hwid = TrickleOrdering::from(other.hardware_id.cmp(&self.hardware_id));
        // Lower chunk counts compare as "greater" so the network will reach a consensus
        // corresponding to the device with the least complete firmware update
        let consider_chunk_count =
            TrickleOrdering::from(other.chunk_count.cmp(&self.chunk_count).reverse());
        consider_hwid.then(consider_chunk_count)
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
                chunk_count: 100,
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
            assert_eq!(write.chunk_count, 100);
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
                chunk_count: 200,
                chunk_index: 150,
                chunk_data,
            }),
        };

        let mut buffer = [0u8; MAX_PACKET_LEN];
        let serialized = state.serialize_packet(&mut buffer);
        dbg!(&serialized);
        let deserialized =
            CommState::try_deserialize_packet(serialized).expect("Failed to deserialize packet");

        assert_eq!(state.seq_num, deserialized.seq_num);
        if let CommType::BlCodeWrite(write) = deserialized.type_ {
            assert_eq!(write.hardware_id, 0xCAFEBABE);
            assert_eq!(write.chunk_count, 200);
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
}
