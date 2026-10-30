#!/usr/bin/env python3
"""yunet.onnx with its input's height and width, and its outputs' lengths,
free, and the shapes inferred from the fixed ones dropped. See the README."""

import sys

import onnx

source, target = sys.argv[1:3]
model = onnx.load(source)
dims = model.graph.input[0].type.tensor_type.shape.dim
for dim, name in ((dims[2], "h"), (dims[3], "w")):
    dim.ClearField("dim_value")
    dim.dim_param = name
for output in model.graph.output:
    for dim in output.type.tensor_type.shape.dim[1:]:
        dim.ClearField("dim_value")
        dim.dim_param = "n" + dim.dim_param
del model.graph.value_info[:]
onnx.save(model, target)
