#!/usr/bin/env python3
"""Content-hash Trunk's Web Worker JS, WASM, and loader shim assets."""

import hashlib
import os
from pathlib import Path


def main() -> None:
    staging_env = os.environ.get("TRUNK_STAGING_DIR")
    if not staging_env:
        raise SystemExit("TRUNK_STAGING_DIR is not set")
    staging = Path(staging_env)

    worker_js = staging / "slice_worker.js"
    worker_wasm = staging / "slice_worker_bg.wasm"
    worker_loader = staging / "slice_worker_loader.js"
    index_html = staging / "index.html"

    for path in (worker_js, worker_wasm, index_html):
        if not path.is_file():
            raise SystemExit("missing expected Trunk staging artifact: " + str(path))

    digest = hashlib.sha256(worker_js.read_bytes() + worker_wasm.read_bytes()).hexdigest()[:16]

    hashed_js_name = "slice_worker-" + digest + ".js"
    hashed_wasm_name = "slice_worker-" + digest + "_bg.wasm"
    hashed_loader_name = "slice_worker_loader-" + digest + ".js"

    worker_js.rename(staging / hashed_js_name)
    worker_wasm.rename(staging / hashed_wasm_name)
    worker_loader.unlink(missing_ok=True)

    loader_body = (
        'importScripts("./' + hashed_js_name + '");'
        'wasm_bindgen({ module_or_path: "./' + hashed_wasm_name + '" })'
        ".catch(function(e){setTimeout(function(){throw e;},0);});\n"
    )
    (staging / hashed_loader_name).write_text(loader_body, encoding="utf-8")

    html = index_html.read_text(encoding="utf-8")
    placeholder = 'content="./slice_worker_loader.js"'
    if placeholder not in html:
        raise SystemExit("placeholder not found in staging index.html: " + placeholder)
    updated_html = html.replace(placeholder, 'content="./' + hashed_loader_name + '"', 1)
    index_html.write_text(updated_html, encoding="utf-8")


if __name__ == "__main__":
    main()
