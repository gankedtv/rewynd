use byteorder::{BigEndian, ReadBytesExt, WriteBytesExt};
use serde::Serialize;
use std::io::{Read, Seek, Write};

use crate::mp4box::*;

/// The `Opus` sample entry (an ISO-BMFF `AudioSampleEntry`) carrying a child `dOps` box.
/// The entry prefix matches `mp4a`; only the codec-specific child differs (`dOps` rather
/// than `esds`). Encapsulation of Opus in ISO-BMFF, RFC 7845 §5.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OpusBox {
    pub data_reference_index: u16,
    pub channelcount: u16,
    pub samplesize: u16,

    #[serde(with = "value_u32")]
    pub samplerate: FixedPointU16,
    pub dops: DopsBox,
}

impl OpusBox {
    pub fn new(config: &OpusConfig) -> Self {
        Self {
            data_reference_index: 1,
            channelcount: u16::from(config.channels),
            samplesize: 16,
            // The sample entry always advertises 48 kHz — Opus decodes at 48 kHz
            // regardless of the source rate, which lives in dOps.input_sample_rate.
            samplerate: FixedPointU16::new(48000),
            dops: DopsBox::new(config),
        }
    }

    pub fn get_type(&self) -> BoxType {
        BoxType::OpusBox
    }

    pub fn get_size(&self) -> u64 {
        HEADER_SIZE + 8 + 20 + self.dops.box_size()
    }
}

impl Mp4Box for OpusBox {
    fn box_type(&self) -> BoxType {
        self.get_type()
    }

    fn box_size(&self) -> u64 {
        self.get_size()
    }

    fn to_json(&self) -> Result<String> {
        Ok(serde_json::to_string(&self).unwrap())
    }

    fn summary(&self) -> Result<String> {
        Ok(format!(
            "channel_count={} sample_size={} sample_rate={}",
            self.channelcount,
            self.samplesize,
            self.samplerate.value()
        ))
    }
}

impl<R: Read + Seek> ReadBox<&mut R> for OpusBox {
    fn read_box(reader: &mut R, size: u64) -> Result<Self> {
        let start = box_start(reader)?;

        reader.read_u32::<BigEndian>()?; // reserved
        reader.read_u16::<BigEndian>()?; // reserved
        let data_reference_index = reader.read_u16::<BigEndian>()?;

        reader.read_u64::<BigEndian>()?; // reserved
        let channelcount = reader.read_u16::<BigEndian>()?;
        let samplesize = reader.read_u16::<BigEndian>()?;
        reader.read_u32::<BigEndian>()?; // pre-defined, reserved
        let samplerate = FixedPointU16::new_raw(reader.read_u32::<BigEndian>()?);

        let BoxHeader { name, size: s } = BoxHeader::read(reader)?;
        if s > size {
            return Err(Error::InvalidData(
                "opus box contains a box with a larger size than it",
            ));
        }
        if name != BoxType::DopsBox {
            return Err(Error::InvalidData("opus sample entry missing its dOps box"));
        }
        let dops = DopsBox::read_box(reader, s)?;

        skip_bytes_to(reader, start + size)?;

        Ok(OpusBox {
            data_reference_index,
            channelcount,
            samplesize,
            samplerate,
            dops,
        })
    }
}

impl<W: Write> WriteBox<&mut W> for OpusBox {
    fn write_box(&self, writer: &mut W) -> Result<u64> {
        let size = self.box_size();
        BoxHeader::new(self.box_type(), size).write(writer)?;

        writer.write_u32::<BigEndian>(0)?; // reserved
        writer.write_u16::<BigEndian>(0)?; // reserved
        writer.write_u16::<BigEndian>(self.data_reference_index)?;

        writer.write_u64::<BigEndian>(0)?; // reserved
        writer.write_u16::<BigEndian>(self.channelcount)?;
        writer.write_u16::<BigEndian>(self.samplesize)?;
        writer.write_u32::<BigEndian>(0)?; // reserved
        writer.write_u32::<BigEndian>(self.samplerate.raw_value())?;

        self.dops.write_box(writer)?;

        Ok(size)
    }
}

/// The `dOps` box (`OpusSpecificBox`, RFC 7845 §5.1). NOT a `FullBox`: the leading byte is
/// this box's own `version` field, not a FullBox version/flags word. All multi-byte fields
/// are big-endian (unlike the little-endian `OpusHead` of Ogg). For
/// `channel_mapping_family == 0` (mono/stereo) there is no trailing channel-mapping table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DopsBox {
    pub version: u8,
    pub output_channel_count: u8,
    pub pre_skip: u16,
    pub input_sample_rate: u32,
    pub output_gain: i16,
    pub channel_mapping_family: u8,
}

impl DopsBox {
    pub fn new(config: &OpusConfig) -> Self {
        Self {
            version: 0,
            output_channel_count: config.channels,
            pre_skip: config.pre_skip,
            input_sample_rate: config.sample_rate,
            output_gain: 0,
            channel_mapping_family: 0,
        }
    }

    pub fn get_type(&self) -> BoxType {
        BoxType::DopsBox
    }

    pub fn get_size(&self) -> u64 {
        // version(1) + output_channel_count(1) + pre_skip(2) + input_sample_rate(4)
        // + output_gain(2) + channel_mapping_family(1) = 11. Family 0 → no mapping table.
        HEADER_SIZE + 11
    }
}

impl Mp4Box for DopsBox {
    fn box_type(&self) -> BoxType {
        self.get_type()
    }

    fn box_size(&self) -> u64 {
        self.get_size()
    }

    fn to_json(&self) -> Result<String> {
        Ok(serde_json::to_string(&self).unwrap())
    }

    fn summary(&self) -> Result<String> {
        Ok(format!(
            "channels={} pre_skip={} input_sample_rate={}",
            self.output_channel_count, self.pre_skip, self.input_sample_rate
        ))
    }
}

impl<R: Read + Seek> ReadBox<&mut R> for DopsBox {
    fn read_box(reader: &mut R, size: u64) -> Result<Self> {
        let start = box_start(reader)?;

        let version = reader.read_u8()?;
        let output_channel_count = reader.read_u8()?;
        let pre_skip = reader.read_u16::<BigEndian>()?;
        let input_sample_rate = reader.read_u32::<BigEndian>()?;
        let output_gain = reader.read_i16::<BigEndian>()?;
        let channel_mapping_family = reader.read_u8()?;

        skip_bytes_to(reader, start + size)?;

        Ok(DopsBox {
            version,
            output_channel_count,
            pre_skip,
            input_sample_rate,
            output_gain,
            channel_mapping_family,
        })
    }
}

impl<W: Write> WriteBox<&mut W> for DopsBox {
    fn write_box(&self, writer: &mut W) -> Result<u64> {
        let size = self.box_size();
        BoxHeader::new(self.box_type(), size).write(writer)?;

        writer.write_u8(self.version)?;
        writer.write_u8(self.output_channel_count)?;
        writer.write_u16::<BigEndian>(self.pre_skip)?;
        writer.write_u32::<BigEndian>(self.input_sample_rate)?;
        writer.write_i16::<BigEndian>(self.output_gain)?;
        writer.write_u8(self.channel_mapping_family)?;

        Ok(size)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mp4box::BoxHeader;
    use std::io::Cursor;

    #[test]
    fn test_dops_exact_bytes() {
        // 48 kHz / stereo / pre_skip=312 / gain=0 / family=0. Byte-for-byte per RFC 7845.
        let dops = DopsBox::new(&OpusConfig {
            channels: 2,
            sample_rate: 48000,
            pre_skip: 312,
        });
        let mut buf = Vec::new();
        dops.write_box(&mut buf).unwrap();
        assert_eq!(
            buf,
            vec![
                0x00, 0x00, 0x00, 0x13, // box size = 19
                0x64, 0x4f, 0x70, 0x73, // 'dOps'
                0x00, // version
                0x02, // output channel count
                0x01, 0x38, // pre_skip = 312
                0x00, 0x00, 0xbb, 0x80, // input sample rate = 48000
                0x00, 0x00, // output gain = 0
                0x00, // channel mapping family = 0
            ]
        );
    }

    #[test]
    fn test_dops_round_trip() {
        let src = DopsBox::new(&OpusConfig {
            channels: 2,
            sample_rate: 48000,
            pre_skip: 312,
        });
        let mut buf = Vec::new();
        src.write_box(&mut buf).unwrap();
        assert_eq!(buf.len(), src.box_size() as usize);

        let mut reader = Cursor::new(&buf);
        let header = BoxHeader::read(&mut reader).unwrap();
        assert_eq!(header.name, BoxType::DopsBox);
        let dst = DopsBox::read_box(&mut reader, header.size).unwrap();
        assert_eq!(src, dst);
    }

    #[test]
    fn test_opus_sample_entry_round_trip() {
        let src = OpusBox::new(&OpusConfig {
            channels: 2,
            sample_rate: 48000,
            pre_skip: 312,
        });
        let mut buf = Vec::new();
        src.write_box(&mut buf).unwrap();
        assert_eq!(buf.len(), src.box_size() as usize);
        assert_eq!(src.box_size(), 55); // 8 header + 28 entry prefix + 19 dOps

        let mut reader = Cursor::new(&buf);
        let header = BoxHeader::read(&mut reader).unwrap();
        assert_eq!(header.name, BoxType::OpusBox);
        let dst = OpusBox::read_box(&mut reader, header.size).unwrap();
        assert_eq!(src, dst);
        // The sample entry advertises 48 kHz regardless of source rate.
        assert_eq!(dst.samplerate.value(), 48000);
    }
}
