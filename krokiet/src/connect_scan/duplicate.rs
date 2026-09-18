use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::thread;

use czkawka_core::common::consts::DEFAULT_THREAD_SIZE;
use czkawka_core::common::model::CheckingMethod;
use czkawka_core::common::tool_data::CommonData;
use czkawka_core::common::traits::{ResultEntry, Search};
use czkawka_core::common::{format_time, split_path, split_path_compare};
use czkawka_core::tools::duplicate;
use czkawka_core::tools::duplicate::{DuplicateEntry, DuplicateFinder, DuplicateFinderParameters};
use humansize::{BINARY, format_size};
use rayon::prelude::*;
use slint::{ComponentHandle, ModelRc, SharedString, VecModel, Weak};

use crate::common::{MAX_INT_DATA_DUPLICATE_FILES, MAX_STR_DATA_DUPLICATE_FILES, split_u64_into_i32s};
use crate::connect_scan::{
    MessagesData, ScanData, get_dt_timestamp_string, get_text_messages, insert_data_to_model, reset_selection_at_end,
    set_common_settings,
};
use crate::{ActiveTab, GuiState, MainWindow, flk};

pub(crate) fn scan_duplicates(a: Weak<MainWindow>, sd: ScanData) {
    thread::Builder::new()
        .stack_size(DEFAULT_THREAD_SIZE)
        .spawn(move || {
            let hash_type = sd.combo_box_items.duplicates_hash_type.value;
            let check_method = sd.combo_box_items.duplicates_check_method.value;

            let params = DuplicateFinderParameters::new(
                check_method,
                hash_type,
                sd.custom_settings.duplicate_use_prehash,
                sd.custom_settings.duplicate_minimal_hash_cache_size as u64,
                sd.custom_settings.duplicate_minimal_prehash_cache_size as u64,
                sd.custom_settings.duplicates_sub_name_case_sensitive,
            );
            let mut tool = DuplicateFinder::new(params);

            set_common_settings(&mut tool, &sd.custom_settings, &sd.stop_flag);
            tool.search(&sd.stop_flag, Some(&sd.progress_sender));
            let (critical, messages) = get_text_messages(&tool, &sd.basic_settings);

            let mut vector;
            if tool.get_use_reference() {
                match tool.get_params().check_method {
                    CheckingMethod::Hash => {
                        vector = tool
                            .get_files_with_identical_hashes_referenced()
                            .values()
                            .flatten()
                            .cloned()
                            .map(|(original, other)| (Some(original), other))
                            .collect::<Vec<_>>();
                    }
                    CheckingMethod::Name | CheckingMethod::Size | CheckingMethod::SizeName => {
                        let values: Vec<_> = match tool.get_params().check_method {
                            CheckingMethod::Name => tool.get_files_with_identical_name_referenced().values().cloned().collect(),
                            CheckingMethod::Size => tool.get_files_with_identical_size_referenced().values().cloned().collect(),
                            CheckingMethod::SizeName => tool.get_files_with_identical_size_names_referenced().values().cloned().collect(),
                            _ => unreachable!("Invalid check method."),
                        };
                        vector = values.into_iter().map(|(original, other)| (Some(original), other)).collect::<Vec<_>>();
                    }
                    _ => unreachable!("Invalid check method."),
                }
            } else {
                match tool.get_params().check_method {
                    CheckingMethod::Hash => {
                        vector = tool.get_files_sorted_by_hash().values().flatten().cloned().map(|items| (None, items)).collect::<Vec<_>>();
                    }
                    CheckingMethod::Name | CheckingMethod::Size | CheckingMethod::SizeName => {
                        let values: Vec<_> = match tool.get_params().check_method {
                            CheckingMethod::Name => tool.get_files_sorted_by_names().values().cloned().collect(),
                            CheckingMethod::Size => tool.get_files_sorted_by_size().values().cloned().collect(),
                            CheckingMethod::SizeName => tool.get_files_sorted_by_size_name().values().cloned().collect(),
                            _ => unreachable!("Invalid check method."),
                        };
                        vector = values.into_iter().map(|items| (None, items)).collect::<Vec<_>>();
                    }
                    _ => unreachable!("Invalid check method."),
                }
            }

            for (_first, vec) in &mut vector {
                vec.par_sort_unstable_by(|a, b| split_path_compare(a.path.as_path(), b.path.as_path()));
            }

            // Read video metadata while we are still on the worker thread.
            // This avoids blocking the Slint UI event loop.
            //
            // In Hash mode every member of a group is byte-identical, so one
            // successful ffprobe result can safely be reused for the full group.
            // In Name/Size/SizeName modes files are only candidates, therefore
            // every video is probed independently.
            let video_durations = collect_video_durations(&vector, check_method);

            let info = tool.get_information();
            let stopped_search = tool.get_stopped_search();
            let (duplicates_number, groups_number, lost_space) = match tool.get_check_method() {
                CheckingMethod::Hash => (info.number_of_duplicated_files_by_hash, info.number_of_groups_by_hash, info.lost_space_by_hash),
                CheckingMethod::Name => (info.number_of_duplicated_files_by_name, info.number_of_groups_by_name, 0),
                CheckingMethod::Size => (info.number_of_duplicated_files_by_size, info.number_of_groups_by_size, info.lost_space_by_size),
                CheckingMethod::SizeName => (info.number_of_duplicated_files_by_size_name, info.number_of_groups_by_size_name, info.lost_space_by_size),
                _ => unreachable!("invalid check method {:?}", tool.get_check_method()),
            };
            sd.shared_models.lock().expect("Mutex poisoned").shared_duplication_state = Some(tool);

            let messages_data = MessagesData { critical, messages };

            a.upgrade_in_event_loop(move |app| {
                write_duplicate_results(&app, vector, video_durations, messages_data, info, sd, stopped_search, duplicates_number, groups_number, lost_space);
            })
        })
        .expect("Cannot start thread - not much we can do here");
}
fn write_duplicate_results(
    app: &MainWindow,
    vector: Vec<(Option<DuplicateEntry>, Vec<DuplicateEntry>)>,
    video_durations: HashMap<PathBuf, i32>,
    messages_data: MessagesData,
    info: duplicate::Info,
    sd: ScanData,
    stopped_search: bool,
    items_found: usize,
    groups: usize,
    lost_space: u64,
) {
    let scanning_time_str = format_time(info.scanning_time);

    let items = Rc::new(VecModel::default());
    for (ref_fe, vec_fe) in vector.into_iter().rev() {
        if let Some(ref_fe) = ref_fe {
            let duration_seconds = duration_for_entry(&video_durations, &ref_fe);
            let (data_model_str, data_model_int) = prepare_data_model_duplicates(ref_fe, duration_seconds);
            insert_data_to_model(&items, data_model_str, data_model_int, Some(true));
        } else {
            insert_data_to_model(&items, ModelRc::new(VecModel::default()), ModelRc::new(VecModel::default()), Some(false));
        }

        for fe in vec_fe {
            let duration_seconds = duration_for_entry(&video_durations, &fe);
            let (data_model_str, data_model_int) = prepare_data_model_duplicates(fe, duration_seconds);
            insert_data_to_model(&items, data_model_str, data_model_int, None);
        }
    }
    app.set_duplicate_files_model(items.into());
    if let Some(critical) = messages_data.critical {
        app.invoke_scan_ended(critical.into());
    } else {
        if !stopped_search && sd.basic_settings.play_audio_on_scan_completion {
            sd.audio_player.play_scan_completed();
        }
        let result_message = if lost_space > 0 {
            flk!(
                "rust_found_duplicate_files",
                items_found = items_found,
                groups = groups,
                size = format_size(lost_space, BINARY),
                time = scanning_time_str
            )
        } else {
            flk!(
                "rust_found_duplicate_files_no_lost_space",
                items_found = items_found,
                groups = groups,
                time = scanning_time_str
            )
        };
        if !stopped_search && sd.basic_settings.show_notification_on_scan_completion {
            crate::notification_manager::send_scan_completed_notification("Duplicate Files", &result_message);
        }
        app.invoke_scan_ended(result_message.into());
    }
    app.global::<GuiState>().set_info_text(messages_data.messages.into());
    reset_selection_at_end(app, ActiveTab::DuplicateFiles);
}
fn prepare_data_model_duplicates(fe: DuplicateEntry, duration_seconds: i32) -> (ModelRc<SharedString>, ModelRc<i32>) {
    let (directory, file) = split_path(fe.get_path());

    let duration_text = if duration_seconds >= 0 {
        format_duration(duration_seconds)
    } else {
        "-".to_string()
    };

    let data_model_str_arr: [SharedString; MAX_STR_DATA_DUPLICATE_FILES] = [
        format_size(fe.size, BINARY).into(),
        file.into(),
        directory.into(),
        duration_text.into(),
        get_dt_timestamp_string(fe.get_modified_date()).into(),
    ];
    let data_model_str = VecModel::from_slice(&data_model_str_arr);
    let modification_split = split_u64_into_i32s(fe.get_modified_date());
    let size_split = split_u64_into_i32s(fe.size);
    let data_model_int_arr: [i32; MAX_INT_DATA_DUPLICATE_FILES] = [modification_split.0, modification_split.1, size_split.0, size_split.1, duration_seconds];
    let data_model_int = VecModel::from_slice(&data_model_int_arr);
    (data_model_str, data_model_int)
}

fn collect_video_durations(
    groups: &[(Option<DuplicateEntry>, Vec<DuplicateEntry>)],
    checking_method: CheckingMethod,
) -> HashMap<PathBuf, i32> {
    let mut result = HashMap::new();

    for (reference, entries) in groups {
        if checking_method == CheckingMethod::Hash {
            // Hash groups are byte-identical. Probe only one video from the group.
            let probe_entry = reference.as_ref().or_else(|| entries.first());

            let Some(probe_entry) = probe_entry else {
                continue;
            };

            if !is_video_file(probe_entry.get_path()) {
                continue;
            }

            let duration = get_video_duration_seconds(probe_entry.get_path()).unwrap_or(-1);

            if let Some(reference) = reference {
                result.insert(reference.get_path().to_path_buf(), duration);
            }

            for entry in entries {
                result.insert(entry.get_path().to_path_buf(), duration);
            }
        } else {
            // Name/Size/SizeName do not guarantee identical content.
            if let Some(reference) = reference {
                insert_duration_for_entry(&mut result, reference);
            }

            for entry in entries {
                insert_duration_for_entry(&mut result, entry);
            }
        }
    }

    result
}

fn insert_duration_for_entry(result: &mut HashMap<PathBuf, i32>, entry: &DuplicateEntry) {
    if !is_video_file(entry.get_path()) {
        return;
    }

    let duration = get_video_duration_seconds(entry.get_path()).unwrap_or(-1);
    result.insert(entry.get_path().to_path_buf(), duration);
}

fn duration_for_entry(video_durations: &HashMap<PathBuf, i32>, entry: &DuplicateEntry) -> i32 {
    video_durations.get(entry.get_path()).copied().unwrap_or(-1)
}

fn is_video_file(path: &Path) -> bool {
    let Some(extension) = path.extension().and_then(|extension| extension.to_str()) else {
        return false;
    };

    matches!(
        extension.to_ascii_lowercase().as_str(),
        "3gp"
            | "avi"
            | "flv"
            | "m2ts"
            | "m4v"
            | "mkv"
            | "mov"
            | "mp4"
            | "mpeg"
            | "mpg"
            | "mts"
            | "ts"
            | "vob"
            | "webm"
            | "wmv"
    )
}

fn get_video_duration_seconds(path: &Path) -> Option<i32> {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
        ])
        .arg(path)
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let text = String::from_utf8_lossy(&output.stdout);
    let seconds = text.trim().parse::<f64>().ok()?;

    if !seconds.is_finite() || seconds < 0.0 || seconds > i32::MAX as f64 {
        return None;
    }

    Some(seconds.round() as i32)
}

fn format_duration(total_seconds: i32) -> String {
    let total_seconds = total_seconds.max(0);
    let hours = total_seconds / 3600;
    let minutes = (total_seconds % 3600) / 60;
    let seconds = total_seconds % 60;

    format!("{hours:02}:{minutes:02}:{seconds:02}")
}
