#![no_std]

use core::array;

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use trickle::{TrickleOrd, TrickleOrdering, TrickleParams};

static CRC: crc::Crc<u32> = crc::Crc::<u32>::new(&crc::CRC_32_BZIP2);

pub const TRICKLE_PARAMS: TrickleParams = TrickleParams {
    i_min_millis: 10,
    i_max_millis: 10_000,
    k: 1,
};

pub const MAX_PACKET_LEN: usize = 300;

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
#[serde(bound(deserialize = "'de: 'a"))]
pub struct CommState<'a> {
    seq_num: u64,
    type_: CommType<'a>,
}

impl<'a> TrickleOrd for CommState<'a> {
    fn consider(&self, other: &Self) -> trickle::TrickleOrdering {
        let consider_seq_num = TrickleOrdering::from(other.seq_num.cmp(&self.seq_num));
        consider_seq_num.then_with(|| self.type_.consider(&other.type_))
    }
}

impl<'a> CommState<'a> {
    pub fn propagate(&self) -> [Self; 4] {
        self.type_.propagate().map(|type_| CommState {
            seq_num: self.seq_num,
            type_,
        })
    }

    pub fn try_deserialize_packet(s: &'a mut [u8]) -> postcard::Result<Self> {
        let sz = cobs::decode_in_place(s).map_err(|_| postcard::Error::DeserializeBadEncoding)?;
        postcard::de_flavors::crc::from_bytes_u32(&s[..sz], CRC.digest())
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
pub const COMM_TYPE_BL_CODE_WRITE: u8 = 0x81;
pub const COMM_TYPE_BL_CODE_PROGRESS: u8 = 0x82;
pub const COMM_TYPE_BL_UNKNOWN: u8 = 0xFF;

pub const COMM_TYPE_BL_BITMASK: u8 = COMM_TYPE_BL_INIT;

#[cfg(feature = "bl")]
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct BlCodeWrite<'a> {
    pub hwid: u32,
    pub chunk_count: u32,
    pub chunk_num: u32,
    pub chunk_data: &'a [u8],
}

#[cfg(feature = "bl")]
impl<'a> BlCodeWrite<'a> {
    pub fn consider(&self, other: &Self) -> TrickleOrdering {
        other.cmp(self).into()
    }
}

#[cfg(feature = "bl")]
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct BlCodeProgress {
    pub hwid: u32,
    pub chunk_count: u32,
}

#[cfg(feature = "bl")]
impl BlCodeProgress {
    pub fn consider(&self, other: &Self) -> TrickleOrdering {
        let consider_hwid = TrickleOrdering::from(other.hwid.cmp(&self.hwid));
        // Lower chunk counts compare as "greater" so the network will reach a consensus
        // corresponding to the device with the least complete firmware update
        let consider_chunk_count =
            TrickleOrdering::from(other.chunk_count.cmp(&self.chunk_count).reverse());
        consider_hwid.then(consider_chunk_count)
    }
}

#[derive(Debug, Clone)]
#[repr(u8)]
pub enum CommType<'a> {
    #[cfg(feature = "app")]
    Init = COMM_TYPE_INIT,
    Unknown = COMM_TYPE_UNKNOWN,
    #[cfg(feature = "bl")]
    BlInit = COMM_TYPE_BL_INIT,
    #[cfg(feature = "bl")]
    BlCodeWrite(BlCodeWrite<'a>) = COMM_TYPE_BL_CODE_WRITE,
    #[cfg(feature = "bl")]
    BlCodeProgress(BlCodeProgress) = COMM_TYPE_BL_CODE_PROGRESS,
    BlUnknown = COMM_TYPE_BL_UNKNOWN,
}

impl<'a> Serialize for CommType<'a> {
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

impl<'de: 'a, 'a> Deserialize<'de> for CommType<'a> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        use serde::de::SeqAccess;

        struct CommTypeVisitor<'a>(core::marker::PhantomData<&'a ()>);

        impl<'de: 'a, 'a> Visitor<'de> for CommTypeVisitor<'a> {
            type Value = CommType<'a>;

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
                        let data: BlCodeWrite<'a> = seq
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

        deserializer.deserialize_any(CommTypeVisitor(core::marker::PhantomData))
    }
}

impl<'a> Default for CommType<'a> {
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

impl<'a> CommType<'a> {
    pub fn discriminant(&self) -> u8 {
        // SAFETY: `Self` is marked `repr(u8)`
        unsafe { *<*const _>::from(self).cast::<u8>() }
    }

    pub fn is_bl(&self) -> bool {
        (self.discriminant() & COMM_TYPE_BL_BITMASK) != 0
    }

    pub fn propagate(&self) -> [Self; 4] {
        match self {
            #[cfg(feature = "app")]
            CommType::Init => array::from_fn(|_| Self::Init),
            CommType::Unknown => array::from_fn(|_| Self::Unknown),
            #[cfg(feature = "bl")]
            CommType::BlInit => array::from_fn(|_| Self::BlInit),
            #[cfg(feature = "bl")]
            CommType::BlCodeWrite(data) => array::from_fn(|_| Self::BlCodeWrite(data.clone())),
            #[cfg(feature = "bl")]
            CommType::BlCodeProgress(data) => {
                array::from_fn(|_| Self::BlCodeProgress(data.clone()))
            }
            CommType::BlUnknown => array::from_fn(|_| Self::BlUnknown),
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
            (CommType::BlCodeWrite(s), CommType::BlCodeWrite(o)) => s.consider(o),
            #[cfg(feature = "bl")]
            (CommType::BlCodeProgress(s), CommType::BlCodeProgress(o)) => s.consider(o),
            (CommType::BlUnknown, CommType::BlUnknown) => TrickleOrdering::Consistent,
            #[cfg(feature = "app")]
            (s, CommType::Unknown) if !s.is_bl() => TrickleOrdering::Consistent,
            (s, CommType::BlUnknown) if s.is_bl() => TrickleOrdering::Consistent,
            (s, o) => o.discriminant().cmp(&s.discriminant()).into(),
        }
    }
}
