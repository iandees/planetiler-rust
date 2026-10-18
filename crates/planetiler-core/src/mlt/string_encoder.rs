//! Port of `org.maplibre.mlt.converter.encodings.StringEncoder`.

use std::collections::HashMap;

use super::encoding::{
    DictionaryType, LengthType, LogicalLevelTechnique, LogicalStreamType, OffsetType, PhysicalLevelTechnique,
    PhysicalStreamType, StreamMetadata, encode_boolean_stream, put_varint,
};
use super::fsst;
use super::integer_encoder::encode_int_stream;

/// `StringEncoder.encodeSharedDictionary`: encodes several string columns (with `None` for missing values) sharing
/// one dictionary. Returns `(number of streams, encoded bytes)`.
pub fn encode_shared_dictionary(values: &[Vec<Option<&str>>], fast_pfor: bool, use_fsst: bool) -> (u32, Vec<u8>) {
    let mut dictionary: Vec<&str> = Vec::new();
    let mut dict_map: HashMap<&str, i32> = HashMap::new();
    let mut data_streams: Vec<Vec<i32>> = Vec::with_capacity(values.len());
    let mut present_streams: Vec<Vec<bool>> = Vec::with_capacity(values.len());
    for column in values {
        let mut present = Vec::with_capacity(column.len());
        let mut data = Vec::with_capacity(column.len());
        for value in column {
            present.push(value.is_some());
            if let Some(v) = value {
                let next = dictionary.len() as i32;
                let idx = *dict_map.entry(v).or_insert_with(|| {
                    dictionary.push(v);
                    next
                });
                data.push(idx);
            }
        }
        present_streams.push(present);
        data_streams.push(data);
    }

    if dictionary.is_empty() {
        // Set number of streams to zero if no columns are present in this tile
        return (0, Vec::new());
    }

    let encoded_shared_dictionary = encode_dictionary(&dictionary, fast_pfor, false, true);
    let encoded_shared_fsst_dictionary = if use_fsst { Some(encode_fsst(&dictionary, fast_pfor, false)) } else { None };
    let using_fsst =
        encoded_shared_fsst_dictionary.as_ref().is_some_and(|fsst| fsst.len() < encoded_shared_dictionary.len());
    let mut result =
        if using_fsst { encoded_shared_fsst_dictionary.expect("checked above") } else { encoded_shared_dictionary };

    for (present, data) in present_streams.iter().zip(&data_streams) {
        if data.is_empty() {
            // If no values are present in this column add zero for number of streams
            put_varint(&mut result, 0);
            continue;
        }
        put_varint(&mut result, 2);
        encode_boolean_stream(&mut result, present, PhysicalStreamType::Present);
        encode_int_stream(
            &mut result,
            data,
            fast_pfor,
            false,
            PhysicalStreamType::Offset,
            Some(LogicalStreamType::Offset(OffsetType::String)),
        );
    }
    // TODO: make present stream optional
    let num_streams = (if using_fsst { 5 } else { 3 }) + values.len() as u32 * 2;
    (num_streams, result)
}

/// `StringEncoder.encode`: the smallest of plain, dictionary and (if enabled) FSST dictionary encodings of a single
/// string column. Returns `(number of streams, encoded bytes)`.
pub fn encode(values: &[&str], fast_pfor: bool, use_fsst: bool) -> (u32, Vec<u8>) {
    let plain = encode_plain(values, fast_pfor);
    let dictionary = encode_dictionary(values, fast_pfor, true, false);
    let fsst_dictionary = if use_fsst { Some(encode_fsst_dictionary(values, fast_pfor)) } else { None };
    // Stream.min keeps the first of equal elements
    let mut best = (2, plain);
    if dictionary.len() < best.1.len() {
        best = (3, dictionary);
    }
    if let Some(fsst) = fsst_dictionary
        && fsst.len() < best.1.len()
    {
        best = (5, fsst);
    }
    best
}

fn encode_fsst_dictionary(values: &[&str], fast_pfor: bool) -> Vec<u8> {
    let mut data_stream: Vec<i32> = Vec::with_capacity(values.len());
    let mut dictionary: Vec<&str> = Vec::new();
    let mut dict_map: HashMap<&str, i32> = HashMap::with_capacity(values.len());
    for &value in values {
        let next = dictionary.len() as i32;
        let idx = *dict_map.entry(value).or_insert_with(|| {
            dictionary.push(value);
            next
        });
        data_stream.push(idx);
    }
    let mut result = encode_fsst(&dictionary, fast_pfor, false);
    encode_int_stream(
        &mut result,
        &data_stream,
        fast_pfor,
        false,
        PhysicalStreamType::Offset,
        Some(LogicalStreamType::Offset(OffsetType::String)),
    );
    result
}

fn encode_fsst(values: &[&str], fast_pfor: bool, is_shared_dictionary: bool) -> Vec<u8> {
    let joined: Vec<u8> = values.iter().flat_map(|v| v.as_bytes().iter().copied()).collect();
    let symbol_table = fsst::encode(&joined);
    let symbol_lengths: Vec<i32> = symbol_table.symbol_lengths.iter().map(|&l| l as i32).collect();
    let mut result = Vec::new();
    encode_int_stream(
        &mut result,
        &symbol_lengths,
        fast_pfor,
        false,
        PhysicalStreamType::Length,
        Some(LogicalStreamType::Length(LengthType::Symbol)),
    );
    StreamMetadata::new(
        PhysicalStreamType::Data,
        Some(LogicalStreamType::Dictionary(DictionaryType::Fsst)),
        LogicalLevelTechnique::None,
        LogicalLevelTechnique::None,
        PhysicalLevelTechnique::None,
        symbol_lengths.len() as u32,
        symbol_table.symbols.len() as u32,
    )
    .encode_into(&mut result);
    result.extend_from_slice(&symbol_table.symbols);
    let length_stream: Vec<i32> = values.iter().map(|v| v.len() as i32).collect();
    encode_int_stream(
        &mut result,
        &length_stream,
        fast_pfor,
        false,
        PhysicalStreamType::Length,
        Some(LogicalStreamType::Length(LengthType::Dictionary)),
    );
    StreamMetadata::new(
        PhysicalStreamType::Data,
        Some(LogicalStreamType::Dictionary(if is_shared_dictionary {
            DictionaryType::Shared
        } else {
            DictionaryType::Single
        })),
        LogicalLevelTechnique::None,
        LogicalLevelTechnique::None,
        PhysicalLevelTechnique::None,
        values.len() as u32,
        symbol_table.compressed_data.len() as u32,
    )
    .encode_into(&mut result);
    result.extend_from_slice(&symbol_table.compressed_data);
    result
}

fn encode_dictionary(
    values: &[&str],
    fast_pfor: bool,
    encode_offset_stream: bool,
    is_shared_dictionary: bool,
) -> Vec<u8> {
    let mut offset_stream: Vec<i32> = Vec::with_capacity(values.len());
    let mut length_stream: Vec<i32> = Vec::new();
    let mut dict_map: HashMap<&str, i32> = HashMap::with_capacity(values.len());
    let mut dictionary_data: Vec<u8> = Vec::new();
    let mut dictionary_size = 0i32;
    for &value in values {
        let idx = *dict_map.entry(value).or_insert_with(|| {
            length_stream.push(value.len() as i32);
            dictionary_data.extend_from_slice(value.as_bytes());
            dictionary_size += 1;
            dictionary_size - 1
        });
        offset_stream.push(idx);
    }
    let mut result = Vec::with_capacity(dictionary_data.len() + offset_stream.len() + length_stream.len() + 16);
    encode_int_stream(
        &mut result,
        &length_stream,
        fast_pfor,
        false,
        PhysicalStreamType::Length,
        Some(LogicalStreamType::Length(LengthType::Dictionary)),
    );
    if encode_offset_stream {
        encode_int_stream(
            &mut result,
            &offset_stream,
            fast_pfor,
            false,
            PhysicalStreamType::Offset,
            Some(LogicalStreamType::Offset(OffsetType::String)),
        );
    }
    StreamMetadata::new(
        PhysicalStreamType::Data,
        Some(LogicalStreamType::Dictionary(if is_shared_dictionary {
            DictionaryType::Shared
        } else {
            DictionaryType::Single
        })),
        LogicalLevelTechnique::None,
        LogicalLevelTechnique::None,
        PhysicalLevelTechnique::None,
        dictionary_size as u32,
        dictionary_data.len() as u32,
    )
    .encode_into(&mut result);
    result.extend_from_slice(&dictionary_data);
    result
}

/// `StringEncoder.encodePlain`.
pub fn encode_plain(values: &[&str], fast_pfor: bool) -> Vec<u8> {
    let length_stream: Vec<i32> = values.iter().map(|v| v.len() as i32).collect();
    let total: usize = values.iter().map(|v| v.len()).sum();
    let mut result = Vec::with_capacity(total + values.len() + 16);
    encode_int_stream(
        &mut result,
        &length_stream,
        fast_pfor,
        false,
        PhysicalStreamType::Length,
        Some(LogicalStreamType::Length(LengthType::VarBinary)),
    );
    StreamMetadata::new(
        PhysicalStreamType::Data,
        Some(LogicalStreamType::Dictionary(DictionaryType::None)),
        LogicalLevelTechnique::None,
        LogicalLevelTechnique::None,
        PhysicalLevelTechnique::None,
        values.len() as u32,
        total as u32,
    )
    .encode_into(&mut result);
    for v in values {
        result.extend_from_slice(v.as_bytes());
    }
    result
}
