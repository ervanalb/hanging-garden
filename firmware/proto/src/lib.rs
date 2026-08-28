#![no_std]

use serde::{Deserialize, Serialize};
use trickle::{TrickleOrd, TrickleOrdering};

static CRC: crc::Crc<u32> = crc::Crc::<u32>::new(&crc::CRC_32_BZIP2);

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct CommState {}

impl TrickleOrd for CommState {
    fn consider(&self, _other: &Self) -> trickle::TrickleOrdering {
        TrickleOrdering::Equal
    }
}

impl CommState {
    pub fn propagate(&self) -> [Self; 4] {
        core::array::from_fn(|_| CommState {})
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
