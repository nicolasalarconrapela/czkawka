#![allow(clippy::needless_pass_by_value)]

mod bad_extensions;
mod bad_names;
mod big_files;
mod broken_files;
mod duplicate;
mod empty_files;
mod empty_folders;
mod exif_remover;
#[cfg(feature = "fast_duplicates")]
pub(crate) mod fast_duplicate_gui;
mod invalid_symlinks;
mod same_music;
mod similar_images;
mod similar_videos;
mod temporary_files;
mod video_optimizer;

#[cfg(feature = "fast_duplicates")]
use std::collections::BTreeMap;
#[cfg(feature = "fast_duplicates")]
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use chrono::{Local, TimeZone, Utc};
use crossbeam_channel::Sender;
#[cfg(feature = "fast_duplicates")]
use czkawka_core::common::config_cache_path::get_config_cache_path;
use czkawka_core::common::progress_data::ProgressData;
use czkawka_core::common::tool_data::CommonData;
use czkawka_core::helpers::messages::MessageLimit;
use slint::{ComponentHandle, Image, ModelRc, SharedString, VecModel};

use crate::audio_player::AudioPlayer;
use crate::common::{check_if_all_included_dirs_are_referenced, check_if_there_are_any_included_folders};
use crate::connect_row_selection::checker::set_number_of_enabled_items;
use crate::connect_row_selection::reset_selection;
use crate::connect_scan::bad_extensions::scan_bad_extensions;
use crate::connect_scan::bad_names::scan_bad_names;
use crate::connect_scan::big_files::scan_big_files;
use crate::connect_scan::broken_files::scan_broken_files;
use crate::connect_scan::duplicate::scan_duplicates;
use crate::connect_scan::empty_files::scan_empty_files;
use crate::connect_scan::empty_folders::scan_empty_folders;
use crate::connect_scan::exif_remover::scan_exif_remover;
use crate::connect_scan::invalid_symlinks::scan_invalid_symlinks;
use crate::connect_scan::same_music::scan_similar_music;
use crate::connect_scan::similar_images::scan_similar_images;
use crate::connect_scan::similar_videos::scan_similar_videos;
use crate::connect_scan::temporary_files::scan_temporary_files;
use crate::connect_scan::video_optimizer::scan_video_optimizer;
use crate::settings::model::{BasicSettings, ComboBoxItems, SettingsCustom};
use crate::settings::{collect_base_settings, collect_combo_box_settings, collect_settings};
use crate::shared_models::SharedModels;
#[cfg(feature = "fast_duplicates")]
use crate::duplicate_engine::{
    CalibrationMode, CalibrationOptions, CalibrationStability, calibrate_and_store_mode,
    storage_key_for_path,
};
use crate::{ActiveTab, Callabler, GuiState, MainWindow, ProgressToSend, SingleMainListModel, flk};

pub struct ScanData {
    pub progress_sender: Sender<ProgressData>,
    pub stop_flag: Arc<AtomicBool>,
    pub custom_settings: SettingsCustom,
    pub basic_settings: BasicSettings,
    pub combo_box_items: ComboBoxItems,
    pub shared_models: Arc<Mutex<SharedModels>>,
    pub audio_player: Arc<AudioPlayer>,
}

pub struct MessagesData {
    pub critical: Option<String>,
    pub messages: String,
}

pub(crate) fn connect_scan_button(
    app: &MainWindow,
    progress_sender: Sender<ProgressData>,
    stop_flag: Arc<AtomicBool>,
    shared_models: Arc<Mutex<SharedModels>>,
    audio_player: Arc<AudioPlayer>,
) {
    let a = app.as_weak();
    app.on_scan_starting(move |active_tab| {
        let app = a.upgrade().expect("Failed to upgrade app :(");

        if !check_if_there_are_any_included_folders(&app) {
            app.invoke_scan_ended(flk!("rust_no_included_paths").into());
            return;
        }

        if check_if_all_included_dirs_are_referenced(&app) {
            app.invoke_scan_ended(flk!("rust_all_paths_referenced").into());
            return;
        }

        let progress_sender = progress_sender.clone();
        let stop_flag = stop_flag.clone();

        app.set_progress_datas(ProgressToSend {
            all_progress: 0,
            current_progress: -1,
            current_progress_size: -1,
            step_name: "".into(),
        });

        let custom_settings = collect_settings(&app);
        let basic_settings = collect_base_settings(&app);
        let combo_box_items = collect_combo_box_settings(&app);

        let cloned_model = Arc::clone(&shared_models);

        app.global::<GuiState>().set_info_text("".into());

        let a = app.as_weak();
        let audio_player_clone = Arc::clone(&audio_player);

        let scan_data = ScanData {
            progress_sender,
            stop_flag,
            custom_settings,
            basic_settings,
            combo_box_items,
            shared_models: cloned_model,
            audio_player: audio_player_clone,
        };

        match active_tab {
            ActiveTab::DuplicateFiles => scan_duplicates(a, scan_data),
            ActiveTab::EmptyFolders => scan_empty_folders(a, scan_data),
            ActiveTab::BigFiles => scan_big_files(a, scan_data),
            ActiveTab::EmptyFiles => scan_empty_files(a, scan_data),
            ActiveTab::SimilarImages => scan_similar_images(a, scan_data),
            ActiveTab::SimilarVideos => scan_similar_videos(a, scan_data),
            ActiveTab::SimilarMusic => scan_similar_music(a, scan_data),
            ActiveTab::InvalidSymlinks => scan_invalid_symlinks(a, scan_data),
            ActiveTab::BadExtensions => scan_bad_extensions(a, scan_data),
            ActiveTab::BadNames => scan_bad_names(a, scan_data),
            ActiveTab::BrokenFiles => scan_broken_files(a, scan_data),
            ActiveTab::TemporaryFiles => scan_temporary_files(a, scan_data),
            ActiveTab::ExifRemover => scan_exif_remover(a, scan_data),
            ActiveTab::VideoOptimizer => scan_video_optimizer(a, scan_data),
            ActiveTab::Settings | ActiveTab::About => panic!("Button should be disabled"),
        }
    });

    connect_fast_engine_calibrator(app);
}


fn connect_fast_engine_calibrator(app: &MainWindow) {
    let weak = app.as_weak();

    app.global::<Callabler>().on_calibrate_fast_engine(move || {
        let Some(app) = weak.upgrade() else {
            return;
        };

        #[cfg(not(feature = "fast_duplicates"))]
        {
            let message =
                "Fast Engine no esta disponible en esta compilacion. Inicia Krokiet con -FastEngine.";
            app.global::<GuiState>().set_info_text(message.into());
            app.invoke_show_error_popup(message.into());
            return;
        }

        #[cfg(feature = "fast_duplicates")]
        {
            if app.get_scanning() || app.get_processing() {
                let message = "No se puede calibrar Fast Engine mientras hay otra operacion en curso.";
                app.global::<GuiState>().set_info_text(message.into());
                app.invoke_show_error_popup(message.into());
                return;
            }

            let settings = collect_settings(&app);
            if settings.included_paths.is_empty() {
                let message = "Añade al menos una ruta incluida antes de calibrar Fast Engine.";
                app.global::<GuiState>().set_info_text(message.into());
                app.invoke_show_error_popup(message.into());
                return;
            }

            let Some(config) = get_config_cache_path() else {
                let message =
                    "Krokiet no pudo obtener su directorio de cache para verifier-tuning.json.";
                app.global::<GuiState>().set_info_text(message.into());
                app.invoke_show_error_popup(message.into());
                return;
            };

            let store_path = config.cache_folder.join("verifier-tuning.json");
            let included_paths = settings.included_paths.clone();
            let event_loop = app.as_weak();

            app.set_processing(true);
            app.set_progress_datas(ProgressToSend {
                all_progress: 0,
                current_progress: -1,
                current_progress_size: -1,
                step_name: "Calibrando Fast Engine".into(),
            });
            app.set_text_summary_text("Calibrando Fast Engine...".into());
            app.global::<GuiState>()
                .set_info_text("Calibracion manual de Fast Engine en curso...".into());
            app.invoke_show_fast_calibration_popup(
                "Preparando calibracion...".into(),
                "Identificando los volumenes incluidos.".into(),
                0,
            );

            thread::Builder::new()
                .name("krokiet-fast-calibration".to_string())
                .spawn(move || {
                    let result = calibrate_fast_engine_paths(&included_paths, &store_path, &event_loop);

                    event_loop
                        .upgrade_in_event_loop(move |app| {
                            app.set_processing(false);
                            app.invoke_hide_fast_calibration_popup();

                            match result {
                                Ok(message) => {
                                    app.set_text_summary_text("Calibracion Fast Engine completada".into());
                                    app.global::<GuiState>().set_info_text(message.clone().into());
                                    app.invoke_show_fast_calibration_result_popup(
                                        true,
                                        "Fast Engine ya tiene un perfil de calibracion disponible.".into(),
                                        message.into(),
                                    );
                                }
                                Err(message) => {
                                    app.set_text_summary_text("Calibracion Fast Engine fallida".into());
                                    app.global::<GuiState>().set_info_text(message.clone().into());
                                    app.invoke_show_fast_calibration_result_popup(
                                        false,
                                        "No se pudo completar una calibracion util.".into(),
                                        message.into(),
                                    );
                                }
                            }
                        })
                        .expect("Failed to publish Fast Engine calibration result");
                })
                .expect("Cannot start Fast Engine calibration thread");
        }
    });
}

#[cfg(feature = "fast_duplicates")]
fn calibrate_fast_engine_paths(paths: &[PathBuf], store_path: &Path, event_loop: &slint::Weak<MainWindow>) -> Result<String, String> {
    let mut storages = BTreeMap::<String, PathBuf>::new();
    let mut path_errors = Vec::new();

    for path in paths {
        let calibration_path = calibration_directory_for(path);
        match storage_key_for_path(&calibration_path) {
            Ok(storage_key) => {
                storages.entry(storage_key).or_insert(calibration_path);
            }
            Err(error) => path_errors.push(format!("{}: {error}", path.display())),
        }
    }

    if storages.is_empty() {
        let detail = if path_errors.is_empty() {
            "no se encontro ningun almacenamiento calibrable".to_string()
        } else {
            path_errors.join("\n")
        };
        return Err(format!("Fast Engine no pudo iniciar la calibracion.\n\n{detail}"));
    }

    let total_storages = storages.len();
    let mut successes = Vec::new();
    let mut failures = path_errors;

    for (index, (storage_key, directory)) in storages.into_iter().enumerate() {
        publish_fast_calibration_progress(
            event_loop,
            index,
            total_storages,
            &storage_key,
            &directory,
            false,
        );

        match calibrate_and_store_mode(
            &directory,
            store_path,
            CalibrationOptions::default(),
            CalibrationMode::Auto,
        ) {
            Ok(run) if run.report.profile.stability != CalibrationStability::Invalid => {
                successes.push(format!(
                    "{} -> {} worker(s), estabilidad {}",
                    storage_key,
                    run.report.profile.workers,
                    run.report.profile.stability.label_es()
                ));
            }
            Ok(run) => failures.push(format!(
                "{} -> calibracion inestable ({} worker(s), no se guarda como perfil util)",
                storage_key, run.report.profile.workers
            )),
            Err(error) => failures.push(format!("{storage_key} -> {error}")),
        }

        publish_fast_calibration_progress(
            event_loop,
            index + 1,
            total_storages,
            &storage_key,
            &directory,
            true,
        );
    }

    if successes.is_empty() {
        return Err(format!(
            "Fast Engine no pudo completar una calibracion valida.\n\n{}",
            failures.join("\n")
        ));
    }

    let mut message = format!(
        "Calibracion Fast Engine completada.\nPerfiles: {}\n{}",
        successes.len(),
        successes.join("\n")
    );

    if !failures.is_empty() {
        message.push_str("\n\nIncidencias:\n");
        message.push_str(&failures.join("\n"));
    }

    message.push_str(&format!("\n\nPerfil: {}", store_path.display()));
    Ok(message)
}

#[cfg(feature = "fast_duplicates")]
fn publish_fast_calibration_progress(
    event_loop: &slint::Weak<MainWindow>,
    completed_storages: usize,
    total_storages: usize,
    storage_key: &str,
    directory: &Path,
    completed_current: bool,
) {
    let total = total_storages.max(1);
    let overall_percent = ((completed_storages.saturating_mul(100)) / total).min(100) as i32;
    let current_number = if completed_current {
        completed_storages.min(total)
    } else {
        (completed_storages + 1).min(total)
    };

    let step_name = if completed_current {
        format!(
            "Fast Engine - volumen {current_number}/{total} completado: {storage_key}"
        )
    } else {
        format!(
            "Calibrando Fast Engine - volumen {current_number}/{total}: {}",
            directory.display()
        )
    };

    let summary = if completed_current {
        format!(
            "Calibracion Fast Engine: {completed_storages}/{total} volumen(es) completado(s)"
        )
    } else {
        format!(
            "Calibrando Fast Engine: volumen {current_number}/{total}"
        )
    };

    let storage_key = storage_key.to_string();

    let _ = event_loop.upgrade_in_event_loop(move |app| {
        app.set_progress_datas(ProgressToSend {
            all_progress: overall_percent,
            current_progress: -1,
            current_progress_size: -1,
            step_name: step_name.clone().into(),
        });
        app.set_text_summary_text(summary.into());
        app.invoke_update_fast_calibration_popup(
            step_name.into(),
            storage_key.into(),
            overall_percent,
        );
    });
}

#[cfg(feature = "fast_duplicates")]
fn calibration_directory_for(path: &Path) -> PathBuf {
    if path.is_dir() {
        path.to_path_buf()
    } else {
        path.parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| path.to_path_buf())
    }
}

fn get_dt_timestamp_string(timestamp: u64) -> String {
    let dt_local = Utc.timestamp_opt(timestamp as i64, 0).single().unwrap_or_default().with_timezone(&Local);
    dt_local.format("%Y-%m-%d %H:%M:%S").to_string()
}

////////////////////////////////////////// Common

fn reset_selection_at_end(app: &MainWindow, active_tab: ActiveTab) {
    reset_selection(app, active_tab, true);
    set_number_of_enabled_items(app, active_tab, 0);
}

fn insert_data_to_model(
    items: &Rc<VecModel<SingleMainListModel>>,
    data_model_str: ModelRc<SharedString>,
    data_model_int: ModelRc<i32>,
    filled_header_row: Option<bool>,
) {
    insert_data_to_model_with_thumbnail(items, data_model_str, data_model_int, Image::default(), filled_header_row);
}

fn insert_data_to_model_with_thumbnail(
    items: &Rc<VecModel<SingleMainListModel>>,
    data_model_str: ModelRc<SharedString>,
    data_model_int: ModelRc<i32>,
    thumbnail: Image,
    filled_header_row: Option<bool>,
) {
    let main = SingleMainListModel {
        checked: false,
        header_row: filled_header_row.is_some(),
        filled_header_row: filled_header_row.unwrap_or(false),
        focused_row: false,
        thumbnail,
        val_str: ModelRc::new(data_model_str),
        val_int: ModelRc::new(data_model_int),
    };
    items.push(main);
}

fn get_text_messages<T>(component: &T, basic_settings: &BasicSettings) -> (Option<String>, String)
where
    T: CommonData,
{
    let limit = if basic_settings.settings_limit_lines_of_messages {
        MessageLimit::Lines(500)
    } else {
        MessageLimit::NoLimit
    };

    let text_messages = component.get_text_messages();
    (text_messages.critical.clone(), text_messages.create_messages_text(limit))
}

fn set_common_settings<T>(component: &mut T, custom_settings: &SettingsCustom, stop_flag: &Arc<AtomicBool>)
where
    T: CommonData,
{
    stop_flag.store(false, Ordering::Relaxed);

    component.set_included_paths(custom_settings.included_paths.clone());
    component.set_reference_paths(custom_settings.included_paths_referenced.clone());
    component.set_excluded_paths(custom_settings.excluded_paths.clone());
    component.set_recursive_search(custom_settings.recursive_search);
    component.set_minimal_file_size(custom_settings.minimum_file_size as u64 * 1024);
    component.set_maximal_file_size(custom_settings.maximum_file_size as u64 * 1024);
    component.set_allowed_extensions(custom_settings.allowed_extensions.split(',').map(str::to_string).collect());
    component.set_excluded_extensions(custom_settings.excluded_extensions.split(',').map(str::to_string).collect());
    component.set_excluded_items(custom_settings.excluded_items.split(',').map(str::to_string).collect());
    component.set_exclude_other_filesystems(custom_settings.ignore_other_file_systems);
    component.set_use_cache(custom_settings.use_cache);
    component.set_save_also_as_json(custom_settings.save_also_as_json);
    component.set_delete_outdated_cache(custom_settings.delete_outdated_cache_entries);
    component.set_hide_hard_links(custom_settings.hide_hard_links);
}
