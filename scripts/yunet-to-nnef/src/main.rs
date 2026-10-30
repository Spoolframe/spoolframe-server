//! See the README: an ONNX file with free dims in, the NNEF tar out.

use tract_onnx::prelude::*;
use tract_onnx_opl::WithOnnx;

/// Height then width; `faces::CANVAS` in the server names the same.
const INPUT: (usize, usize) = (384, 640);

fn main() -> TractResult<()> {
    let usage = "usage: yunet-to-nnef <in.onnx> <out.nnef.tar>";
    let mut args = std::env::args().skip(1);
    let (onnx, nnef) = (args.next().expect(usage), args.next().expect(usage));
    let model = tract_onnx::onnx()
        .model_for_path(&onnx)?
        .with_input_fact(0, f32::fact([1, 3, INPUT.0, INPUT.1]).into())?
        .into_typed()?
        .into_decluttered()?;
    tract_nnef::nnef()
        .with_onnx()
        .write_to_tar(&model, std::fs::File::create(&nnef)?)?;
    println!("{nnef}: {} bytes", std::fs::metadata(&nnef)?.len());
    Ok(())
}
