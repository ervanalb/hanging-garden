#![no_std]

use core::array;

use serde::{Deserialize, Serialize};
use trickle::{TrickleOrd, TrickleOrdering, TrickleParams};

static CRC: crc::Crc<u32> = crc::Crc::<u32>::new(&crc::CRC_32_BZIP2);

pub const TRICKLE_PARAMS: TrickleParams = TrickleParams {
    i_min_millis: 10,
    i_max_millis: 10_000,
    k: 1,
};

pub const MAX_PACKET_LEN: usize = 300;

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct CommState {
    seq_num: u64,
    type_: CommType,
}

impl TrickleOrd for CommState {
    fn consider(&self, other: &Self) -> trickle::TrickleOrdering {
        let consider_seq_num = TrickleOrdering::from(other.seq_num.cmp(&self.seq_num));
        consider_seq_num.then_with(|| self.type_.consider(&other.type_))
    }
}

impl CommState {
    pub fn propagate(&self) -> [Self; 4] {
        self.type_.propagate().map(|type_| CommState {
            seq_num: self.seq_num,
            type_,
        })
    }

    pub fn try_deserialize_packet<'a>(s: &'a mut [u8]) -> postcard::Result<Self> {
        let sz = cobs::decode_in_place(s).map_err(|_| postcard::Error::DeserializeBadEncoding)?;
        postcard::de_flavors::crc::from_bytes_u32(&s[..sz], CRC.digest())
    }

    pub fn serialize_packet<'a>(self: &'a Self, s: &'a mut [u8]) -> &'a mut [u8] {
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
pub const COMM_TYPE_NOP: u8 = 0x01;
pub const COMM_TYPE_BL_INIT: u8 = 0x40;
pub const COMM_TYPE_BL_NOP: u8 = 0x41;
pub const COMM_TYPE_BL_CODE_WRITE: u8 = 0x42;
pub const COMM_TYPE_BL_CODE_PROGRESS: u8 = 0x43;

pub const COMM_TYPE_BL_BITMASK: u8 = COMM_TYPE_BL_INIT;

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
#[repr(u8)]
pub enum CommType {
    #[default]
    Init = COMM_TYPE_INIT,
    Nop = COMM_TYPE_NOP,
    BlInit = COMM_TYPE_BL_INIT,
    BlCodeWrite = COMM_TYPE_BL_CODE_WRITE,
    BlCodeProgress = COMM_TYPE_BL_CODE_PROGRESS,
    BlNop = COMM_TYPE_BL_NOP,
}

impl CommType {
    pub fn discriminant(&self) -> u8 {
        // SAFETY: `Self` is marked `repr(u8)`
        unsafe { *<*const _>::from(self).cast::<u8>() }
    }

    pub fn is_bl(&self) -> bool {
        (self.discriminant() & COMM_TYPE_BL_BITMASK) != 0
    }

    pub fn propagate(&self) -> [Self; 4] {
        match self {
            CommType::Init => array::from_fn(|_| Self::Init),
            CommType::Nop => array::from_fn(|_| Self::Nop),
            CommType::BlInit => array::from_fn(|_| Self::BlInit),
            CommType::BlCodeWrite => array::from_fn(|_| Self::BlCodeWrite),
            CommType::BlCodeProgress => array::from_fn(|_| Self::BlCodeProgress),
            CommType::BlNop => array::from_fn(|_| Self::BlNop),
        }
    }

    pub fn consider(&self, other: &Self) -> TrickleOrdering {
        match (self, other) {
            (CommType::Init, CommType::Init) => TrickleOrdering::Consistent,
            (CommType::Nop, CommType::Nop) => TrickleOrdering::Consistent,
            (CommType::BlInit, CommType::BlInit) => TrickleOrdering::Consistent,
            (CommType::BlCodeWrite, CommType::BlCodeWrite) => TrickleOrdering::Consistent,
            (CommType::BlCodeProgress, CommType::BlCodeProgress) => TrickleOrdering::Consistent,
            (CommType::BlNop, CommType::BlNop) => TrickleOrdering::Consistent,
            (s, CommType::Nop) if !s.is_bl() => TrickleOrdering::Consistent,
            (s, CommType::BlNop) if s.is_bl() => TrickleOrdering::Consistent,
            (s, o) => o.discriminant().cmp(&s.discriminant()).into(),
        }
    }
}
