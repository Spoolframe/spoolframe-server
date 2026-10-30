# yunet-to-nnef

Makes `crates/spoolframe-server/assets/yunet-2023mar.nnef.tar`, the model
`crates/spoolframe-server/src/faces.rs` embeds (PX-64). A checked-in step,
not part of the build: the server has no ONNX parser, and this crate is
not in the workspace.

1. Download `face_detection_yunet_2023mar.onnx` from OpenCV Zoo,
   `models/face_detection_yunet`, SHA-256
   `8f2383e4dd3cfbb4553ea8718107fc0423210dc964f9f4280604804ed2552fa4`.
2. Clear the dims the file fixes at 640 × 640 (needs the `onnx` Python
   package): `python3 free-dims.py yunet.onnx yunet-free.onnx`
3. `cargo run --release -- yunet-free.onnx yunet-2023mar.nnef.tar`

The output's input is fixed at 1 × 3 × 384 × 640. tract 0.23.9 can load a
free `h` and `w` from the ONNX file but panics writing them as NNEF, so the
size is set before writing.
