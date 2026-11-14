/// H264 SPS VUI rewriter — ported from `SPSVUIRewriter.ts`.
///
/// Rewrites the VUI (Video Usability Information) section of an H264 SPS
/// NALU to set `max_num_reorder_frames = 0`, which is required for real-time
/// streaming over Discord.
///
/// Based on the WebRTC project reference implementation:
/// <https://webrtc.googlesource.com/src/+/5f2c9278/common_video/h264/sps_vui_rewriter.cc>
use super::annexb_rw::{AnnexBBitstreamReader, AnnexBBitstreamWriter};

/// Rewrite the VUI of an SPS starting at its NAL header; a malformed SPS comes back unchanged.
pub fn rewrite_sps_vui(buffer: &[u8]) -> Vec<u8> {
    try_rewrite_sps_vui(buffer).unwrap_or_else(|| buffer.to_vec())
}

fn try_rewrite_sps_vui(buffer: &[u8]) -> Option<Vec<u8>> {
    if buffer.len() < 2 {
        return None;
    }
    let mut reader = AnnexBBitstreamReader::new(&buffer[1..]);
    let mut writer = AnnexBBitstreamWriter::new();

    // Helpers (mirror TS aliases)
    macro_rules! rb {
        ($n:expr) => {
            reader.read_bits($n)
        };
    }
    macro_rules! wb {
        ($v:expr, $n:expr) => {
            writer.write_bits($v, $n)
        };
    }
    macro_rules! ru {
        ($n:expr) => {
            reader.read_unsigned($n)
        };
    }
    macro_rules! wu {
        ($v:expr, $n:expr) => {
            writer.write_unsigned($v, $n)
        };
    }
    macro_rules! rue {
        () => {
            reader.read_ue()
        };
    }
    macro_rules! wue {
        ($v:expr) => {
            writer.write_ue($v)
        };
    }
    macro_rules! rse {
        () => {
            reader.read_se()
        };
    }
    macro_rules! wse {
        ($v:expr) => {
            writer.write_se($v)
        };
    }

    // NAL header byte (already consumed by the reader's slice offset 1)
    wu!(buffer[0] as u32, 8);

    let profile_idc = ru!(8);
    wu!(profile_idc, 8);
    let constraint_flags = ru!(8);
    wu!(constraint_flags, 8);
    let level_idc = ru!(8);
    wu!(level_idc, 8);

    let seq_parameter_set_id = rue!();
    wue!(seq_parameter_set_id);

    const HIGH_PROFILES: &[u32] = &[100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 144];
    if HIGH_PROFILES.contains(&profile_idc) {
        let chroma_format_idc = rue!();
        wue!(chroma_format_idc);

        if chroma_format_idc == 3 {
            let separate_colour_plane_flag = rb!(1);
            wb!(separate_colour_plane_flag, 1);
        }

        let bit_depth_luma_minus8 = rue!();
        wue!(bit_depth_luma_minus8);
        let bit_depth_chroma_minus8 = rue!();
        wue!(bit_depth_chroma_minus8);

        let qpprime_y_zero_transform_bypass_flag = rb!(1);
        wb!(qpprime_y_zero_transform_bypass_flag, 1);

        let seq_scaling_matrix_present_flag = rb!(1);
        wb!(seq_scaling_matrix_present_flag, 1);
        if seq_scaling_matrix_present_flag != 0 {
            let scaling_count = if chroma_format_idc != 3 { 8 } else { 12 };
            for i in 0..scaling_count {
                let seq_scaling_list_present_flag = rb!(1);
                wb!(seq_scaling_list_present_flag, 1);
                if seq_scaling_list_present_flag != 0 {
                    let size = if i < 6 { 16 } else { 64 };
                    let mut last_scale: i32 = 8;
                    let mut next_scale: i32 = 8;
                    for _ in 0..size {
                        if next_scale != 0 {
                            let delta = rse!();
                            if !(-128..=127).contains(&delta) {
                                return None;
                            }
                            wse!(delta);
                            next_scale = (last_scale + delta + 256) % 256;
                        }
                        if next_scale != 0 {
                            last_scale = next_scale;
                        }
                    }
                }
            }
        }
    }

    let log2_max_frame_num_minus4 = rue!();
    wue!(log2_max_frame_num_minus4);

    let pic_order_cnt_type = rue!();
    wue!(pic_order_cnt_type);
    if pic_order_cnt_type == 0 {
        let log2_max_pic_order_cnt_lsb_minus4 = rue!();
        wue!(log2_max_pic_order_cnt_lsb_minus4);
    } else if pic_order_cnt_type == 1 {
        let delta_pic_order_always_zero_flag = rb!(1);
        wb!(delta_pic_order_always_zero_flag, 1);
        let offset_for_non_ref_pic = rse!();
        wse!(offset_for_non_ref_pic);
        let offset_for_top_to_bottom_field = rse!();
        wse!(offset_for_top_to_bottom_field);
        let num_ref_frames_in_pic_order_cnt_cycle = rue!();
        wue!(num_ref_frames_in_pic_order_cnt_cycle);
        if num_ref_frames_in_pic_order_cnt_cycle > 255 {
            return None;
        }
        for _ in 0..num_ref_frames_in_pic_order_cnt_cycle {
            let offset_for_ref_frame = rse!();
            wse!(offset_for_ref_frame);
        }
    }

    let max_num_ref_frames = rue!();
    wue!(max_num_ref_frames);

    let gaps_in_frame_num_value_allowed_flag = rb!(1);
    wb!(gaps_in_frame_num_value_allowed_flag, 1);

    let pic_width_in_mbs_minus1 = rue!();
    wue!(pic_width_in_mbs_minus1);
    let pic_height_in_map_units_minus1 = rue!();
    wue!(pic_height_in_map_units_minus1);

    let frame_mbs_only_flag = rb!(1);
    wb!(frame_mbs_only_flag, 1);
    if frame_mbs_only_flag == 0 {
        let mb_adaptive_frame_field_flag = rb!(1);
        wb!(mb_adaptive_frame_field_flag, 1);
    }

    let direct_8x8_inference_flag = rb!(1);
    wb!(direct_8x8_inference_flag, 1);

    let frame_cropping_flag = rb!(1);
    wb!(frame_cropping_flag, 1);
    if frame_cropping_flag != 0 {
        let frame_crop_left_offset = rue!();
        wue!(frame_crop_left_offset);
        let frame_crop_right_offset = rue!();
        wue!(frame_crop_right_offset);
        let frame_crop_top_offset = rue!();
        wue!(frame_crop_top_offset);
        let frame_crop_bottom_offset = rue!();
        wue!(frame_crop_bottom_offset);
    }

    // ---------------------------------------------------------------------------
    // VUI parameters
    // ---------------------------------------------------------------------------

    let vui_parameters_present_flag = rb!(1);
    wb!(1, 1); // always write VUI = present

    if vui_parameters_present_flag == 0 {
        // No VUI — write a minimal one
        wb!(0, 2); // aspect_ratio_info_present_flag=0, overscan_info_present_flag=0
        wb!(0, 1); // video_signal_type_present_flag=0
        wb!(0, 5); // chroma_loc_info, timing, nal_hrd, vcl_hrd, pic_struct all 0
        wb!(1, 1); // bitstream_restriction_flag=1
        write_bitstream_restriction(&mut writer, max_num_ref_frames);
    } else {
        // Copy existing VUI, patching the bitstream restriction block
        let aspect_ratio_info_present_flag = rb!(1);
        wb!(aspect_ratio_info_present_flag, 1);
        if aspect_ratio_info_present_flag != 0 {
            let aspect_ratio_idc = ru!(8);
            wu!(aspect_ratio_idc, 8);
            if aspect_ratio_idc == 255 {
                let sar_width = ru!(16);
                wu!(sar_width, 16);
                let sar_height = ru!(16);
                wu!(sar_height, 16);
            }
        }

        let overscan_info_present_flag = rb!(1);
        wb!(overscan_info_present_flag, 1);
        if overscan_info_present_flag != 0 {
            let overscan_appropriate_flag = rb!(1);
            wb!(overscan_appropriate_flag, 1);
        }

        // video_signal_type: read but strip (write 0)
        let video_signal_type_present_flag = rb!(1);
        wb!(0, 1); // strip video signal type
        if video_signal_type_present_flag != 0 {
            let _video_format = rb!(3);
            let _video_full_range_flag = rb!(1);
            let colour_description_present_flag = rb!(1);
            if colour_description_present_flag != 0 {
                let _colour_primaries = ru!(8);
                let _transfer_characteristics = ru!(8);
                let _matrix_coeffs = ru!(8);
            }
        }

        let chroma_loc_info_present_flag = rb!(1);
        wb!(chroma_loc_info_present_flag, 1);
        if chroma_loc_info_present_flag != 0 {
            let chroma_sample_loc_type_top_field = rue!();
            wue!(chroma_sample_loc_type_top_field);
            let chroma_sample_loc_type_bottom_field = rue!();
            wue!(chroma_sample_loc_type_bottom_field);
        }

        let timing_info_present_flag = rb!(1);
        wb!(timing_info_present_flag, 1);
        if timing_info_present_flag != 0 {
            let num_units_in_tick = ru!(32);
            wu!(num_units_in_tick, 32);
            let time_scale = ru!(32);
            wu!(time_scale, 32);
            let fixed_frame_rate_flag = rb!(1);
            wb!(fixed_frame_rate_flag, 1);
        }

        let nal_hrd_parameters_present_flag = rb!(1);
        wb!(nal_hrd_parameters_present_flag, 1);
        if nal_hrd_parameters_present_flag != 0 {
            copy_hrd_parameters(&mut reader, &mut writer)?;
        }

        let vcl_hrd_parameters_present_flag = rb!(1);
        wb!(vcl_hrd_parameters_present_flag, 1);
        if vcl_hrd_parameters_present_flag != 0 {
            copy_hrd_parameters(&mut reader, &mut writer)?;
        }

        if nal_hrd_parameters_present_flag != 0 || vcl_hrd_parameters_present_flag != 0 {
            let low_delay_hrd_flag = rb!(1);
            wb!(low_delay_hrd_flag, 1);
        }

        let pic_struct_present_flag = rb!(1);
        wb!(pic_struct_present_flag, 1);

        let bitstream_restriction_flag = rb!(1);
        wb!(1, 1); // always write bitstream_restriction = present

        if bitstream_restriction_flag == 0 {
            write_bitstream_restriction(&mut writer, max_num_ref_frames);
        } else {
            // Copy most fields but force reorder=0 and buffering=max_num_ref_frames
            let motion_vectors_over_pic_boundaries_flag = rb!(1);
            wb!(motion_vectors_over_pic_boundaries_flag, 1);
            let max_bytes_per_pic_denom = rue!();
            wue!(max_bytes_per_pic_denom);
            let max_bits_per_mb_denom = rue!();
            wue!(max_bits_per_mb_denom);
            let log2_max_mv_length_horizontal = rue!();
            wue!(log2_max_mv_length_horizontal);
            let log2_max_mv_length_vertical = rue!();
            wue!(log2_max_mv_length_vertical);
            let _num_reorder_frames = rue!();
            wue!(0); // force max_num_reorder_frames = 0
            let _max_dec_frame_buffering = rue!();
            wue!(max_num_ref_frames); // force max_dec_frame_buffering
        }
    }

    wb!(1, 1); // rbsp_stop_one_bit
    if reader.overrun() {
        return None;
    }
    writer.flush_final();
    Some(writer.to_vec())
}

/// Write a default `bitstream_restriction()` block.
fn write_bitstream_restriction(writer: &mut AnnexBBitstreamWriter, max_num_ref_frames: u32) {
    writer.write_bits(1, 1); // motion_vectors_over_pic_boundaries_flag (default 1)
    writer.write_ue(2); // max_bytes_per_pic_denom (default 2)
    writer.write_ue(1); // max_bits_per_mb_denom (default 1)
    writer.write_ue(16); // log2_max_mv_length_horizontal (default 16)
    writer.write_ue(16); // log2_max_mv_length_vertical (default 16)
    writer.write_ue(0); // max_num_reorder_frames = 0 (IMPORTANT!)
    writer.write_ue(max_num_ref_frames); // max_dec_frame_buffering
}

/// Copy an `hrd_parameters()` block verbatim.
fn copy_hrd_parameters(
    reader: &mut AnnexBBitstreamReader<'_>,
    writer: &mut AnnexBBitstreamWriter,
) -> Option<()> {
    let cpb_cnt_minus1 = reader.read_ue();
    writer.write_ue(cpb_cnt_minus1);
    if cpb_cnt_minus1 > 31 {
        return None;
    }
    let bit_rate_scale = reader.read_bits(4);
    writer.write_bits(bit_rate_scale, 4);
    let cpb_size_scale = reader.read_bits(4);
    writer.write_bits(cpb_size_scale, 4);
    for _ in 0..=cpb_cnt_minus1 {
        let bit_rate_value_minus1 = reader.read_ue();
        writer.write_ue(bit_rate_value_minus1);
        let cpb_size_value_minus1 = reader.read_ue();
        writer.write_ue(cpb_size_value_minus1);
        let cbr_flag = reader.read_bits(1);
        writer.write_bits(cbr_flag, 1);
    }
    for _ in 0..4 {
        // initial_cpb_removal_delay_length_minus1, cpb_removal_delay_length_minus1,
        // dpb_output_delay_length_minus1, time_offset_length — each 5 bits
        let v = reader.read_bits(5);
        writer.write_bits(v, 5);
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn baseline_720p_sps_without_vui() -> Vec<u8> {
        let mut w = AnnexBBitstreamWriter::new();
        w.write_bits(0x67, 8);
        w.write_bits(66, 8);
        w.write_bits(0xC0, 8);
        w.write_bits(31, 8);
        for v in [0, 0, 0, 2, 1] {
            w.write_ue(v);
        }
        w.write_bits(0, 1);
        w.write_ue(79);
        w.write_ue(44);
        w.write_bits(0b1100, 4);
        w.write_bits(1, 1);
        w.flush_final();
        w.to_vec()
    }

    fn parse_vui_reorder(sps: &[u8]) -> (u32, u32) {
        let mut r = AnnexBBitstreamReader::new(&sps[1..]);
        r.read_bits(24);
        r.read_ue();
        r.read_ue();
        assert_eq!(r.read_ue(), 0, "poc type");
        r.read_ue();
        let max_num_ref_frames = r.read_ue();
        r.read_bits(1);
        r.read_ue();
        r.read_ue();
        assert_eq!(r.read_bits(1), 1, "frame_mbs_only");
        r.read_bits(1);
        assert_eq!(r.read_bits(1), 0, "cropping");
        assert_eq!(r.read_bits(1), 1, "vui present");
        assert_eq!(
            r.read_bits(8),
            0,
            "no aspect/overscan/signal/chroma/timing/hrd/pic_struct"
        );
        assert_eq!(r.read_bits(1), 1, "bitstream_restriction");
        r.read_bits(1);
        r.read_ue();
        r.read_ue();
        r.read_ue();
        r.read_ue();
        let reorder = r.read_ue();
        let dec_buffering = r.read_ue();
        assert!(!r.overrun());
        assert_eq!(dec_buffering, max_num_ref_frames);
        (reorder, max_num_ref_frames)
    }

    #[test]
    fn adds_bitstream_restriction_with_zero_reorder() {
        let sps = baseline_720p_sps_without_vui();
        let out = rewrite_sps_vui(&sps);
        assert_eq!(out[..4], sps[..4]);
        assert_eq!(parse_vui_reorder(&out).0, 0);
    }

    #[test]
    fn matches_js_reference_output() {
        let sps = baseline_720p_sps_without_vui();
        assert_eq!(sps, [0x67, 0x42, 0xC0, 0x1F, 0xED, 0x00, 0xA0, 0x0B, 0x72]);
        // Output of rewriteSPSVUI() from @dank074/discord-video-stream 6.0.0 for the same input.
        let js = [
            0x67, 0x42, 0xC0, 0x1F, 0xED, 0x00, 0xA0, 0x0B, 0x74, 0x03, 0x68, 0x22, 0x11, 0xA8,
        ];
        assert_eq!(rewrite_sps_vui(&sps), js);
    }

    #[test]
    fn rewrite_is_idempotent() {
        let once = rewrite_sps_vui(&baseline_720p_sps_without_vui());
        assert_eq!(rewrite_sps_vui(&once), once);
    }

    #[test]
    fn malformed_sps_is_returned_unchanged() {
        for sps in [
            &[0x67][..],
            &[0x67, 0x64, 0x00],
            &[0x67, 0x42, 0xC0, 0x1F, 0x00, 0x00],
        ] {
            assert_eq!(rewrite_sps_vui(sps), sps);
        }
    }

    fn high_sps_with_terminated_scaling_list(with_vui: bool) -> Vec<u8> {
        let mut w = AnnexBBitstreamWriter::new();
        w.write_bits(0x67, 8);
        w.write_bits(100, 8);
        w.write_bits(0, 8);
        w.write_bits(40, 8);
        for v in [0, 1, 0, 0] {
            w.write_ue(v);
        }
        w.write_bits(0, 1);
        w.write_bits(1, 1);
        w.write_bits(1, 1);
        w.write_se(-8);
        w.write_bits(0, 7);
        for v in [0, 0, 2, 1] {
            w.write_ue(v);
        }
        w.write_bits(0, 1);
        w.write_ue(79);
        w.write_ue(44);
        w.write_bits(0b110, 3);
        if with_vui {
            w.write_bits(1, 1);
            w.write_bits(0, 8);
            w.write_bits(0b11, 2);
            for v in [2, 1, 16, 16, 0, 1] {
                w.write_ue(v);
            }
        } else {
            w.write_bits(0, 1);
        }
        w.write_bits(1, 1);
        w.flush_final();
        w.to_vec()
    }

    #[test]
    fn scaling_list_stops_reading_after_zero_next_scale() {
        assert_eq!(
            rewrite_sps_vui(&high_sps_with_terminated_scaling_list(false)),
            high_sps_with_terminated_scaling_list(true)
        );
    }

    #[test]
    fn exp_golomb_round_trips_extremes() {
        let mut w = AnnexBBitstreamWriter::new();
        for v in [0, 1, i32::MAX, i32::MIN + 1, -5] {
            w.write_se(v);
        }
        w.write_ue(u32::MAX - 1);
        w.write_bits(1, 1);
        w.flush_final();
        let buf = w.to_vec();
        let mut r = AnnexBBitstreamReader::new(&buf);
        for v in [0, 1, i32::MAX, i32::MIN + 1, -5] {
            assert_eq!(r.read_se(), v);
        }
        assert_eq!(r.read_ue(), u32::MAX - 1);
        assert!(!r.overrun());
    }
}
