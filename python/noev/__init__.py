"""Python bindings for the Noev hybrid classical-quantum language.

>>> import noev
>>> program = noev.Program.compile_source('''
... fn ansatz(theta: Float) -> Qubit {
...     let q = |0>
...     q.ry(theta)
...     mv q
... }
... fn energy(theta: Float) -> Float {
...     <ansatz(theta)| Z(0) |ansatz(theta)>
... }
... ''')
>>> rt = noev.Runtime(program)
>>> round(rt.call("energy", 0.0), 6)
1.0
"""

from noev._noev import (
    CompileError,
    Distribution,
    Enum,
    HostContext,
    NoevError,
    Observable,
    Program,
    Runtime,
    RuntimeError,
    Struct,
    ValidationError,
    __version__,
)

__all__ = [
    "CompileError",
    "Distribution",
    "Enum",
    "HostContext",
    "NoevError",
    "Observable",
    "Program",
    "Runtime",
    "RuntimeError",
    "Struct",
    "ValidationError",
    "__version__",
]
