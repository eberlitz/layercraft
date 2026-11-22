//! LayerCraft WebAssembly browser application (`eframe` WebRunner + WebGPU/WebGL2).

#[cfg(target_arch = "wasm32")]
fn main() {
    use layercraft_ui_egui::LayerCraftApp;
    use layercraft_viewport_gpu::install_renderer;
    use wasm_bindgen::JsCast as _;

    wasm_bindgen_futures::spawn_local(async {
        let Some(window) = web_sys::window() else {
            return;
        };
        let Some(doc) = window.document() else {
            return;
        };
        let Some(el) = doc.get_element_by_id("layercraft_canvas") else {
            return;
        };
        let Ok(canvas) = el.dyn_into::<web_sys::HtmlCanvasElement>() else {
            return;
        };

        let web_options = eframe::WebOptions::default();
        let runner = eframe::WebRunner::new();
        let _ = runner
            .start(
                canvas,
                web_options,
                Box::new(|cc| {
                    if let Some(rs) = cc.wgpu_render_state.as_ref() {
                        install_renderer(rs);
                    }
                    Ok(Box::new(LayerCraftApp::default()))
                }),
            )
            .await;
    });
}

#[cfg(not(target_arch = "wasm32"))]
fn main() {}
