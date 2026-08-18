// src/loc_story.rs
//! Wave 5 — StoryTimeline dialogue localization + clip/voice retiming.
//! Registers ONE callback on the Phase-3 AssetBundle dispatcher for `Gallop.StoryTimelineData`
//! (no new game hooks). Ported from the upstream StoryTimelineData.on_LoadAsset hook, adapted to this codebase:
//!  * lists via crate::il2cpp::IList (raw _items/_size — no managed get_Item, no MethodInfo* ABI),
//!  * value-type (i32 timing) fields via il2cpp::get/set_field_i32 (plain stores),
//!  * Il2CppString* pokes via il2cpp::set_field_object -> wbarrier_set (GC-safe),
//!  * config/dict via crate::localize::data(); wrap via crate::wrap.
//! FIX vs upstream: the FontSize read uses `clip_data`, not `this` (mod.rs:222 bug).
//! Runs on the asset-load thread (IL2CPP-attached); offsets/classes are resolved once at install
//! into read-only statics, config is snapshotted per call, so concurrent loader threads are safe.

#![allow(dead_code, non_snake_case)]

use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

use crate::assets::{self, ASSET_PATH_PREFIX};
use crate::il2cpp::{self, Class, IList, Object};
use crate::wrap;

fn log(m: &str) {
    crate::tools::log(m);
}

// ── constants (upstream retiming invariants) ──
const CLIP_TEXT_LINE_WIDTH: i32 = 21;
const CLIP_TEXT_LINE_COUNT: i32 = 3;
const CLIP_TEXT_FONT_SIZE_DEFAULT: i32 = 42;
const STORY_VIEW_CLIP_TEXT_LINE_WIDTH: i32 = 32;
const FONT_SIZE_DEFAULT: i32 = 0; // FontSize.Default (Large=1, Small=2, BoldCaption=3)

// ── serde (tlg-compatible: snake_case native + PascalCase aliases) ──
#[derive(Serialize, Deserialize, Default)]
struct StoryTimelineDataDict {
    #[serde(alias = "Title")]
    title: Option<String>,
    #[serde(alias = "TextBlockList", default)]
    text_block_list: Vec<TextBlockDict>,
    #[serde(default)]
    no_wrap: bool,
}

#[derive(Serialize, Deserialize, Default)]
struct TextBlockDict {
    #[serde(alias = "Name")]
    name: Option<String>,
    #[serde(alias = "Text")]
    text: Option<String>,
    #[serde(alias = "ChoiceDataList", default)]
    choice_data_list: Vec<String>,
    #[serde(alias = "ColorTextInfoList", default)]
    color_text_info_list: Vec<String>,
    new_clip_length: Option<i32>, // per-block override; bypasses WaitFrame+max(typewrite,Voice)
}

// ── resolved-once binding table (offsets already include the 0x10 header) ──
#[derive(Default)]
struct Offsets {
    tc_class: usize, // StoryTimelineTextClipData Class* (for get_text_clip type check)
    // StoryTimelineData
    tcps: usize,
    title: usize,
    block_list: usize,
    length: usize,
    // StoryTimelineTextClipData
    tc_name: usize,
    tc_text: usize,
    tc_size: usize,
    tc_choice_list: usize,
    tc_color_list: usize,
    tc_wait: usize,
    tc_voice: usize,
    choice_text: usize, // nested ChoiceData.Text
    color_text: usize,  // nested ColorTextInfo.Text
    // StoryTimelineBlockData
    bd_text_track: usize,
    bd_block_len: usize,
    bd_chara_list: usize,
    bd_se_list: usize,
    // StoryTimelineClipData
    clip_len: usize,
    clip_start: usize,
    // StoryTimelineTrackData
    track_clip_list: usize,
    // StoryTimelineCharaTrackData — every *MotionTrackData field
    motion_offsets: Vec<usize>,
}
static OFFSETS: OnceLock<Offsets> = OnceLock::new();

/// Drop everything after the last '.' (keeps directories): "story/data/02/1001/foo.bar" -> ".../foo".
fn path_basename(s: &str) -> &str {
    match s.rfind('.') {
        Some(i) => &s[..i],
        None => s,
    }
}

/// new_string(text) stored into `owner.field` through the GC write barrier. (No template::eval here —
/// the upstream story path pokes raw dict text; use crate::template::eval(text) first if a pack needs
/// $(...) expansion in story lines.)
fn set_string_field(owner: Object, offset: usize, text: &str) {
    let s = il2cpp::new_string(text);
    il2cpp::set_field_object(owner, offset, s);
}

fn get_typewrite_length(text_len: usize, tcps: f32) -> i32 {
    (text_len as f32 / tcps * 30.0).round() as i32 // len / cps * fps
}

/// TextTrack -> ClipList[0], verified to be a StoryTimelineTextClipData. Mirrors get_text_clip.
fn get_text_clip(o: &Offsets, block_data: Object) -> Option<Object> {
    let text_track = il2cpp::get_field_object(block_data, o.bd_text_track);
    if text_track.is_null() {
        return None;
    }
    let clip_list = IList::new(il2cpp::get_field_object(text_track, o.track_clip_list))?;
    let clip_data = clip_list.get(0)?;
    if il2cpp::object_class(clip_data) != o.tc_class as Class {
        return None;
    }
    Some(clip_data)
}

/// apply_clip_length: extend the text clip + its block, then the LAST motion clip (unconditional) and
/// the LAST screen-effect clip (only if it reaches block end). Returns new_block_len.
fn apply_clip_length(
    o: &Offsets,
    clip_data: Object,
    orig_clip_len: i32,
    new_clip_len: i32,
    block_data: Object,
    orig_block_len: i32,
) -> i32 {
    il2cpp::set_field_i32(clip_data, o.clip_len, new_clip_len);
    let new_block_len = il2cpp::get_field_i32(clip_data, o.clip_start) + new_clip_len + 1; // +1 spacer frame
    il2cpp::set_field_i32(block_data, o.bd_block_len, new_block_len);
    let clip_len_diff = new_clip_len - orig_clip_len;

    // Motion tracks: LAST clip of every *MotionTrackData track under every CharacterTrackList entry.
    if let Some(chara_list) = IList::new(il2cpp::get_field_object(block_data, o.bd_chara_list)) {
        for chara in chara_list.iter() {
            for &moff in &o.motion_offsets {
                let motion_track = il2cpp::get_field_object(chara, moff);
                if motion_track.is_null() {
                    continue;
                }
                let Some(clips) = IList::new(il2cpp::get_field_object(motion_track, o.track_clip_list)) else {
                    continue;
                };
                let n = clips.count();
                if n == 0 {
                    continue;
                }
                let Some(last) = clips.get(n - 1) else { continue; };
                let l = il2cpp::get_field_i32(last, o.clip_len);
                il2cpp::set_field_i32(last, o.clip_len, l + clip_len_diff); // unconditional
            }
        }
    }
    // Screen effects: LAST clip, only if start_frame + orig_len >= orig_block_len (pre-retime length).
    if let Some(se_list) = IList::new(il2cpp::get_field_object(block_data, o.bd_se_list)) {
        for se in se_list.iter() {
            let Some(clips) = IList::new(il2cpp::get_field_object(se, o.track_clip_list)) else { continue; };
            let n = clips.count();
            if n == 0 {
                continue;
            }
            let Some(last) = clips.get(n - 1) else { continue; };
            let start_frame = il2cpp::get_field_i32(last, o.clip_start);
            let orig_se = il2cpp::get_field_i32(last, o.clip_len);
            if start_frame + orig_se < orig_block_len {
                continue; // mid-block one-shot; leave it
            }
            il2cpp::set_field_i32(last, o.clip_len, orig_se + clip_len_diff);
        }
    }
    new_block_len
}

/// Fallback when there is no dict and story_tcps_multiplier < 1.0: retime by pure typewrite length
/// (raw UTF-16 char count incl. tags; no WaitFrame/VoiceLength terms). Adds new_block_len for grown
/// blocks, orig_block_len otherwise; block 0 counted as-is.
fn adjust_clips_length_with_tcps(o: &Offsets, this: Object, tcps: f32) {
    let Some(block_list) = IList::new(il2cpp::get_field_object(this, o.block_list)) else { return; };
    let mut total_len = 0i32;
    for (raw_i, block_data) in block_list.iter().enumerate() {
        let orig_block_len = il2cpp::get_field_i32(block_data, o.bd_block_len);
        if raw_i == 0 {
            total_len += orig_block_len;
            continue;
        }
        let Some(clip_data) = get_text_clip(o, block_data) else {
            total_len += orig_block_len;
            continue;
        };
        let text_obj = il2cpp::get_field_object(clip_data, o.tc_text);
        if text_obj.is_null() {
            total_len += orig_block_len;
            continue;
        }
        let text_len = il2cpp::read_string(text_obj).chars().count(); // scalar count (matches the upstream port source), tags included
        let new_clip_len = get_typewrite_length(text_len, tcps);
        let orig_clip_len = il2cpp::get_field_i32(clip_data, o.clip_len);
        if new_clip_len > orig_clip_len {
            total_len += apply_clip_length(o, clip_data, orig_clip_len, new_clip_len, block_data, orig_block_len);
        } else {
            total_len += orig_block_len;
        }
    }
    il2cpp::set_field_i32(this, o.length, total_len);
}

/// THE dispatcher callback (registered for Gallop.StoryTimelineData). `this` = the asset object.
fn on_load_asset(_bundle: Object, this: Object, name: &str) {
    let Some(o) = OFFSETS.get() else { return; };
    if !name.starts_with(ASSET_PATH_PREFIX) {
        return;
    }
    let ld = crate::localize::data();
    let config = &ld.config;

    // 1. tcps multiplier
    let mut tcps = il2cpp::get_field_i32(this, o.tcps) as f32;
    let tcps_mult = config.story_tcps_multiplier;
    if tcps_mult != 1.0 {
        tcps = (tcps * tcps_mult).round();
        il2cpp::set_field_i32(this, o.tcps, tcps as i32);
    }

    // 2. dict (auto-TL / Sugoi is non-core → stubbed: missing dict just falls back to tcps retime)
    let base_path = path_basename(&name[ASSET_PATH_PREFIX.len()..]);
    let dict_path = format!("{base_path}.json");
    let dict: StoryTimelineDataDict = match ld.load_assets_dict(&dict_path) {
        Some(d) => d,
        None => {
            if tcps_mult < 1.0 {
                adjust_clips_length_with_tcps(o, this, tcps);
            }
            return;
        }
    };

    // 3. story-view vs frame-view (base_path[11..] is ASCII-safe after "story/data/")
    let is_story_view = base_path.starts_with("story/data/") && {
        let rest = &base_path[11..];
        rest.starts_with("02/") || rest.starts_with("04/") || rest.starts_with("09/")
    };

    if let Some(title) = &dict.title {
        set_string_field(this, o.title, title);
    }
    let Some(block_list) = IList::new(il2cpp::get_field_object(this, o.block_list)) else { return; };

    // 4. wrap params
    let mut line_count = CLIP_TEXT_LINE_COUNT;
    if let Some(offset) = config.story_line_count_offset {
        line_count += offset;
    }
    let mut font_size = CLIP_TEXT_FONT_SIZE_DEFAULT;
    let mut line_width = CLIP_TEXT_LINE_WIDTH;
    let mut story_view_line_width = STORY_VIEW_CLIP_TEXT_LINE_WIDTH;
    if let Some(mult) = config.text_frame_font_size_multiplier {
        font_size = (font_size as f32 * mult).round() as i32;
        line_width = (line_width as f32 / mult).round() as i32;
        story_view_line_width = (story_view_line_width as f32 / mult).round() as i32;
    }

    // 5. per-block
    let mut total_len = 0i32;
    let mut total_len_changed = false;
    for (raw_i, block_data) in block_list.iter().enumerate() {
        let orig_block_len = il2cpp::get_field_i32(block_data, o.bd_block_len);
        total_len += orig_block_len;
        if raw_i == 0 {
            continue; // block 0 is the empty spacer: counted, never patched
        }
        let i = raw_i - 1; // dict index = block index - 1
        let Some(text_block_dict) = dict.text_block_list.get(i) else {
            log(&format!("[loc/story] text block {i} not in dict: {dict_path}"));
            break;
        };
        let Some(clip_data) = get_text_clip(o, block_data) else { continue; };

        if let Some(nm) = &text_block_dict.name {
            set_string_field(clip_data, o.tc_name, nm);
        }
        if let Some(text) = &text_block_dict.text {
            let mut modified_text: Option<String> = None;
            if !dict.no_wrap {
                if is_story_view {
                    if let Some(wrapped) = wrap::wrap_text(text, story_view_line_width) {
                        modified_text = Some(wrapped.join(" \n")); // extra space: vertical log ignores \n
                    }
                } else {
                    let size = il2cpp::get_field_i32(clip_data, o.tc_size); // FIX: clip_data (upstream passed `this`)
                    if size == FONT_SIZE_DEFAULT {
                        if let Some(fitted) = wrap::wrap_fit_text(text, line_width, line_count, font_size) {
                            modified_text = Some(fitted);
                        }
                    }
                }
            }
            let new_text = modified_text.as_deref().unwrap_or(text.as_str());
            set_string_field(clip_data, o.tc_text, new_text);

            if config.auto_adjust_story_clip_length || text_block_dict.new_clip_length.is_some() || tcps_mult < 1.0 {
                let new_clip_len = text_block_dict.new_clip_length.unwrap_or_else(|| {
                    let text_len = wrap::IsolateTags::new(new_text).fold(0usize, |acc, (s, is_not_tag)| {
                        if is_not_tag {
                            acc + s.chars().count()
                        } else {
                            acc
                        }
                    });
                    let typewrite_len = get_typewrite_length(text_len, tcps);
                    il2cpp::get_field_i32(clip_data, o.tc_wait)
                        + typewrite_len.max(il2cpp::get_field_i32(clip_data, o.tc_voice))
                });
                let orig_clip_len = il2cpp::get_field_i32(clip_data, o.clip_len);
                if new_clip_len > orig_clip_len {
                    let new_block_len =
                        apply_clip_length(o, clip_data, orig_clip_len, new_clip_len, block_data, orig_block_len);
                    total_len += new_block_len - orig_block_len;
                    total_len_changed = true;
                }
            }
        }

        // choices (index-aligned; empty string = keep original; missing dict entry = warn)
        if let Some(choices) = IList::new(il2cpp::get_field_object(clip_data, o.tc_choice_list)) {
            for (j, choice) in choices.iter().enumerate() {
                match text_block_dict.choice_data_list.get(j) {
                    Some(t) if !t.is_empty() => set_string_field(choice, o.choice_text, t),
                    Some(_) => {}
                    None => log(&format!("[loc/story] choice {j} of block {i} not in dict: {dict_path}")),
                }
            }
        }
        // color text
        if let Some(colors) = IList::new(il2cpp::get_field_object(clip_data, o.tc_color_list)) {
            for (j, cti) in colors.iter().enumerate() {
                match text_block_dict.color_text_info_list.get(j) {
                    Some(t) if !t.is_empty() => set_string_field(cti, o.color_text, t),
                    Some(_) => {}
                    None => log(&format!("[loc/story] color text {j} of block {i} not in dict: {dict_path}")),
                }
            }
        }
    }

    if total_len_changed {
        il2cpp::set_field_i32(this, o.length, total_len);
    }
}

/// Resolve classes + all field offsets ONCE, then register on the dispatcher. Call AFTER
/// assets::install (dispatcher armed). Err (no registration) if any class/field is missing — better
/// to run untranslated than to poke a garbage offset and desync voice.
pub fn install() -> Result<(), String> {
    let std_c = il2cpp::class("Gallop.StoryTimelineData");
    let tc_c = il2cpp::class("Gallop.StoryTimelineTextClipData");
    let bd_c = il2cpp::class("Gallop.StoryTimelineBlockData");
    let clip_c = il2cpp::class("Gallop.StoryTimelineClipData");
    let track_c = il2cpp::class("Gallop.StoryTimelineTrackData");
    let chara_c = il2cpp::class("Gallop.StoryTimelineCharaTrackData");
    if std_c.is_null() || tc_c.is_null() || bd_c.is_null() || clip_c.is_null() || track_c.is_null() || chara_c.is_null() {
        return Err("StoryTimeline classes not resolved".into());
    }
    let choice_c = il2cpp::nested_in(tc_c, "ChoiceData");
    let color_c = il2cpp::nested_in(tc_c, "ColorTextInfo");
    if choice_c.is_null() || color_c.is_null() {
        return Err("StoryTimelineTextClipData nested ChoiceData/ColorTextInfo not resolved".into());
    }

    macro_rules! off {
        ($k:expr, $f:literal) => {
            il2cpp::field_offset($k, $f).ok_or_else(|| format!("missing field {}", $f))?
        };
    }

    let o = Offsets {
        tc_class: tc_c as usize,
        tcps: off!(std_c, "TypewriteCountPerSecond"),
        title: off!(std_c, "Title"),
        block_list: off!(std_c, "BlockList"),
        length: off!(std_c, "Length"),
        tc_name: off!(tc_c, "Name"),
        tc_text: off!(tc_c, "Text"),
        tc_size: off!(tc_c, "Size"),
        tc_choice_list: off!(tc_c, "ChoiceDataList"),
        tc_color_list: off!(tc_c, "ColorTextInfoList"),
        tc_wait: off!(tc_c, "WaitFrame"),
        tc_voice: off!(tc_c, "VoiceLength"),
        choice_text: off!(choice_c, "Text"),
        color_text: off!(color_c, "Text"),
        bd_text_track: off!(bd_c, "TextTrack"),
        bd_block_len: off!(bd_c, "BlockLength"),
        bd_chara_list: off!(bd_c, "CharacterTrackList"),
        bd_se_list: off!(bd_c, "ScreenEffectTrackList"),
        clip_len: off!(clip_c, "ClipLength"),
        clip_start: off!(clip_c, "StartFrame"),
        track_clip_list: off!(track_c, "ClipList"),
        motion_offsets: il2cpp::field_offsets_ending_with(chara_c, "MotionTrackData"),
    };
    if o.motion_offsets.is_empty() {
        log("[loc/story] warning: no *MotionTrackData fields on StoryTimelineCharaTrackData (motion clips won't stretch)");
    }
    let _ = OFFSETS.set(o);
    assets::register_asset_localizer(std_c, on_load_asset);
    Ok(())
}