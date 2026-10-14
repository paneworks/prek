//! Pull-based transfer of one approved Lua snapshot. Each chunk carries the
//! authenticated session fence; source changes retire the whole assembly.
use super::{Definition, MAX_SNAPSHOT_BYTES};
use crate::protocol::{self, Fence, Reply};
use std::{io, time::Duration};
pub const CHUNK_BYTES: usize = 32 * 1024;
pub const TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) mod chunk_bytes {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use serde::{Deserialize, Deserializer, Serializer, de::Error};
    pub fn serialize<S: Serializer>(data: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&STANDARD.encode(data))
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        let encoded = String::deserialize(deserializer)?;
        if encoded.len() > super::CHUNK_BYTES.div_ceil(3) * 4 {
            return Err(D::Error::custom("Lua source chunk exceeds encoded limit"));
        }
        let data = STANDARD.decode(encoded).map_err(D::Error::custom)?;
        if data.len() > super::CHUNK_BYTES {
            return Err(D::Error::custom("Lua source chunk exceeds decoded limit"));
        }
        Ok(data)
    }
}

pub fn chunk(fence: Fence, source: Option<&[u8]>, offset: usize) -> io::Result<Reply> {
    let source = source.unwrap_or_default();
    if source.len() > MAX_SNAPSHOT_BYTES
        || offset > source.len()
        || !offset.is_multiple_of(CHUNK_BYTES)
        || offset == source.len() && offset != 0
    {
        return Err(protocol::invalid("invalid Lua snapshot offset or size"));
    }
    Ok(Reply::LuaSource {
        fence,
        offset,
        bytes: source.len(),
        data: source[offset..(offset + CHUNK_BYTES).min(source.len())].into(),
    })
}
pub struct Assembly {
    fence: Fence,
    data: Vec<u8>,
    bytes: Option<usize>,
    expires: Duration,
}
impl Assembly {
    pub fn new(fence: Fence, now: Duration) -> io::Result<Self> {
        fence.validate()?;
        Ok(Self {
            fence,
            data: Vec::new(),
            bytes: None,
            expires: now + TIMEOUT,
        })
    }
    pub fn offset(&self) -> usize {
        self.data.len()
    }
    pub fn deadline(&self) -> Duration {
        self.expires
    }
    pub fn push(
        &mut self,
        fence: Fence,
        offset: usize,
        bytes: usize,
        data: &[u8],
        now: Duration,
    ) -> io::Result<Option<Option<Definition>>> {
        if fence != self.fence
            || now >= self.expires
            || offset != self.data.len()
            || bytes > MAX_SNAPSHOT_BYTES
            || self.bytes.is_some_and(|expected| expected != bytes)
            || offset > bytes
            || data.len() != CHUNK_BYTES.min(bytes - offset)
            || bytes != 0 && data.is_empty()
        {
            return Err(protocol::invalid(
                "stale, reordered or invalid Lua snapshot chunk",
            ));
        }
        self.bytes = Some(bytes);
        self.data.extend_from_slice(data);
        if self.data.len() < bytes {
            return Ok(None);
        }
        if bytes == 0 {
            return Ok(Some(None));
        }
        let definition: Definition =
            serde_json::from_slice(&self.data).map_err(io::Error::other)?;
        definition.validate().map_err(io::Error::other)?;
        Ok(Some(Some(definition)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fence() -> Fence {
        Fence {
            broker: crate::protocol::Epoch([1, 2]),
            configuration: 1,
            session: 1,
        }
    }
    #[test]
    fn ordered_snapshot_chunks_preserve_the_approved_source() {
        let source = format!(
            "--{}\nlocal p=require('prek');p.events.on('ready',function()end);return {{version=1}}",
            "source ".repeat(8000)
        );
        let definition = Definition::compile(&source, "approved.lua").unwrap();
        let bytes = serde_json::to_vec(&definition).unwrap();
        let mut assembly = Assembly::new(fence(), Duration::ZERO).unwrap();
        loop {
            let Reply::LuaSource {
                fence,
                offset,
                bytes: total,
                data,
            } = chunk(fence(), Some(&bytes), assembly.offset()).unwrap()
            else {
                unreachable!()
            };
            if let Some(Some(decoded)) = assembly
                .push(fence, offset, total, &data, Duration::ZERO)
                .unwrap()
            {
                assert_eq!(decoded, definition);
                break;
            }
        }
    }
    #[test]
    fn old_fences_timeouts_offsets_and_oversized_headers_are_rejected() {
        let now = Duration::ZERO;
        let mut assembly = Assembly::new(fence(), now).unwrap();
        let mut stale = fence();
        stale.configuration += 1;
        assert!(assembly.push(stale, 0, 0, &[], now).is_err());
        assert!(assembly.push(fence(), 1, 1, &[], now).is_err());
        assert!(
            assembly
                .push(fence(), 0, MAX_SNAPSHOT_BYTES + 1, &[], now)
                .is_err()
        );
        assert!(assembly.push(fence(), 0, 0, &[], TIMEOUT).is_err());
        assert!(matches!(
            assembly.push(fence(), 0, 0, &[], now).unwrap(),
            Some(None)
        ));
    }
}
