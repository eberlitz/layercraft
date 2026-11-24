//! Dedicated browser Web Worker executing CPU slicing & G-code emission off the UI thread.

#[cfg(target_arch = "wasm32")]
mod wasm {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use layercraft_engine::worker_protocol::{SliceJobEvent, SliceJobRequest, execute_slice_job};
    use layercraft_slice::SliceProgress;
    use wasm_bindgen::JsCast as _;

    fn post_event(ev: &SliceJobEvent) {
        let Ok(json) = serde_json::to_string(ev) else {
            return;
        };
        if let Ok(scope) = js_sys::global().dyn_into::<web_sys::DedicatedWorkerGlobalScope>() {
            let _ = scope.post_message(&wasm_bindgen::JsValue::from_str(&json));
        }
    }

    pub fn run() {
        let Ok(scope) = js_sys::global().dyn_into::<web_sys::DedicatedWorkerGlobalScope>() else {
            return;
        };

        let on_message =
            wasm_bindgen::closure::Closure::wrap(Box::new(move |ev: web_sys::MessageEvent| {
                let Some(raw) = ev.data().as_string() else {
                    return;
                };
                let req: SliceJobRequest = match serde_json::from_str(&raw) {
                    Ok(r) => r,
                    Err(e) => {
                        post_event(&SliceJobEvent::Error {
                            job_id: 0,
                            message: format!("invalid slice request: {e}"),
                        });
                        return;
                    }
                };
                let job_id = req.job_id;
                let last_sent = Arc::new(AtomicUsize::new(usize::MAX));
                let last_sent_cb = Arc::clone(&last_sent);
                let progress = SliceProgress::with_callback(move |done, total| {
                    let stride = (total / 40).max(1);
                    let prev = last_sent_cb.load(Ordering::Relaxed);
                    let should_send = done == 0
                        || done >= total
                        || prev == usize::MAX
                        || done.saturating_sub(prev) >= stride;
                    if should_send {
                        last_sent_cb.store(done, Ordering::Relaxed);
                        post_event(&SliceJobEvent::Progress {
                            job_id,
                            completed_layers: done,
                            total_layers: total,
                        });
                    }
                });

                let t0 = web_time::Instant::now();
                match execute_slice_job(&req, Some(&progress)) {
                    Ok((plate, gcode)) => {
                        let elapsed_ms = t0.elapsed().as_secs_f64() * 1000.0;
                        post_event(&SliceJobEvent::Done {
                            job_id,
                            elapsed_ms,
                            plate,
                            gcode,
                        });
                    }
                    Err(message) => {
                        post_event(&SliceJobEvent::Error { job_id, message });
                    }
                }
            })
                as Box<dyn FnMut(web_sys::MessageEvent)>);

        scope.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
        on_message.forget();
        post_event(&SliceJobEvent::Ready);
    }
}

#[cfg(target_arch = "wasm32")]
fn main() {
    wasm::run();
}

#[cfg(not(target_arch = "wasm32"))]
fn main() {}
