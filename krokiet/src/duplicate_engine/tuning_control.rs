//! GUI-only, thread-local control for standalone verifier calibration.
//! Normal scan and CLI code paths see no observer and retain their prior behavior.

use std::cell::RefCell;
use std::io as calibration_io;
use std::sync::Arc as CalibrationArc;
use std::sync::atomic::{AtomicBool as CalibrationAtomicBool, Ordering as CalibrationOrdering};

struct CalibrationUiContext {
    cancelled: CalibrationArc<CalibrationAtomicBool>,
    observer: Box<dyn Fn(i32, &'static str)>,
    last_percent: i32,
    segment_start: i32,
    segment_span: i32,
    completed: usize,
    estimated_total: usize,
}

thread_local! {
    static CALIBRATION_UI_CONTEXT: RefCell<Option<CalibrationUiContext>> =
        const { RefCell::new(None) };
}

struct CalibrationUiGuard(Option<CalibrationUiContext>);

impl Drop for CalibrationUiGuard {
    fn drop(&mut self) {
        CALIBRATION_UI_CONTEXT.with(|slot| {
            *slot.borrow_mut() = self.0.take();
        });
    }
}

/// Install a one-off observer for the current calibration thread.
/// The guard restores any prior context on success, error or panic.
pub(crate) fn with_calibration_ui_control<T>(
    cancelled: CalibrationArc<CalibrationAtomicBool>,
    observer: impl Fn(i32, &'static str) + 'static,
    operation: impl FnOnce() -> calibration_io::Result<T>,
) -> calibration_io::Result<T> {
    let previous = CALIBRATION_UI_CONTEXT.with(|slot| {
        slot.replace(Some(CalibrationUiContext {
            cancelled,
            observer: Box::new(observer),
            last_percent: 0,
            segment_start: 10,
            segment_span: 55,
            completed: 0,
            estimated_total: 1,
        }))
    });
    let _guard = CalibrationUiGuard(previous);
    calibration_ui_checkpoint()?;
    calibration_ui_progress(0, "Preparando calibracion");
    operation()
}

fn calibration_ui_checkpoint() -> calibration_io::Result<()> {
    let cancelled = CALIBRATION_UI_CONTEXT.with(|slot| {
        slot.borrow().as_ref().map(|ctx| ctx.cancelled.clone())
    });
    if cancelled.is_some_and(|flag| flag.load(CalibrationOrdering::Relaxed)) {
        return Err(calibration_io::Error::new(
            calibration_io::ErrorKind::Interrupted,
            "verifier calibration cancelled by user",
        ));
    }
    Ok(())
}

fn calibration_ui_progress(percent: i32, stage: &'static str) {
    CALIBRATION_UI_CONTEXT.with(|slot| {
        if let Some(ctx) = slot.borrow_mut().as_mut() {
            let percent = percent.clamp(ctx.last_percent, 99);
            ctx.last_percent = percent;
            (ctx.observer)(percent, stage);
        }
    });
}

/// Work denominator is an upper estimate because rechecks, pairwise checks,
/// and the optional rescue pass may do less work than their upper bounds.
fn calibration_ui_begin_pass(pass: usize, candidates: usize, options: CalibrationOptions) {
    CALIBRATION_UI_CONTEXT.with(|slot| {
        if let Some(ctx) = slot.borrow_mut().as_mut() {
            ctx.segment_start = if pass == 0 { 10 } else { 70 };
            ctx.segment_span = if pass == 0 { 59 } else { 24 };
            ctx.completed = 0;
            ctx.estimated_total = candidates
                .saturating_mul(options.rounds)
                .saturating_add(candidates.saturating_mul(options.recheck_rounds))
                .saturating_add(
                    candidates.saturating_sub(1)
                        .saturating_mul(options.pair_rounds)
                        .saturating_mul(2),
                )
                .max(1);
            let pct = ctx.segment_start.max(ctx.last_percent).min(99);
            ctx.last_percent = pct;
            (ctx.observer)(pct, if pass == 0 {
                "Mediciones iniciales"
            } else {
                "Reevaluacion por inestabilidad"
            });
        }
    });
}

/// Called after a real measured verifier window, not by any timer.
fn calibration_ui_measured_window() {
    CALIBRATION_UI_CONTEXT.with(|slot| {
        if let Some(ctx) = slot.borrow_mut().as_mut() {
            ctx.completed = ctx.completed.saturating_add(1);
            let fraction = (ctx.completed as u64)
                .saturating_mul(ctx.segment_span as u64)
                / ctx.estimated_total as u64;
            let pct = ctx.segment_start.saturating_add(fraction as i32)
                .max(ctx.last_percent).min(94);
            ctx.last_percent = pct;
            (ctx.observer)(pct, "Medicion ExactVerifier");
        }
    });
}

#[cfg(test)]
mod calibration_ui_tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    #[test]
    fn cancellation_interrupts_at_checkpoint() {
        let flag = CalibrationArc::new(CalibrationAtomicBool::new(true));
        let result = with_calibration_ui_control(flag, |_, _| {}, calibration_ui_checkpoint);
        assert_eq!(result.unwrap_err().kind(), calibration_io::ErrorKind::Interrupted);
    }

    #[test]
    fn reported_percentages_do_not_decrease_or_hit_one_hundred_early() {
        let flag = CalibrationArc::new(CalibrationAtomicBool::new(false));
        let values = Rc::new(RefCell::new(Vec::<i32>::new()));
        let sink = values.clone();
        with_calibration_ui_control(flag, move |p, _| sink.borrow_mut().push(p), || {
            calibration_ui_begin_pass(0, 2, CalibrationOptions::default());
            for _ in 0..8 {
                calibration_ui_measured_window();
            }
            calibration_ui_begin_pass(1, 2, CalibrationOptions::default());
            calibration_ui_measured_window();
            Ok(())
        }).unwrap();
        let items = values.borrow();
        assert!(items.windows(2).all(|pair| pair[1] >= pair[0]));
        assert!(items.iter().all(|p| (0..=99).contains(p)));
    }
}
