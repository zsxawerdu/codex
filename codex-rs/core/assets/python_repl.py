import ast
import asyncio
import inspect
import json
import os
import sys
import traceback
import tty

# Unified exec keeps stdin open for terminals. Raw mode avoids echo and the
# canonical line length limit when receiving a multi-line cell as JSON.
tty.setraw(sys.stdin.fileno())
wire = sys.stdin
marker_output = sys.stdout
sys.stdin = open(os.devnull, encoding="utf-8")
namespace = {"__name__": "__main__", "_codex_display": sys.displayhook}

loop = asyncio.new_event_loop()
asyncio.set_event_loop(loop)
ready = f"\n[Python REPL ready {sys.argv.pop()}]"
try:
    print(ready, file=marker_output, flush=True)
    for line in wire.buffer:
        try:
            tree = ast.parse(json.loads(line), filename="<python_repl>")
            if tree.body and isinstance(tree.body[-1], ast.Expr):
                last = tree.body[-1]
                last.value = ast.copy_location(
                    ast.Call(ast.Name("_codex_display", ast.Load()), [last.value], []),
                    last.value,
                )
                ast.fix_missing_locations(last)
            cell = compile(tree, "<python_repl>", "exec", flags=ast.PyCF_ALLOW_TOP_LEVEL_AWAIT)
            result = eval(cell, namespace)
            if inspect.iscoroutine(result):
                loop.run_until_complete(result)
        except SystemExit:
            raise
        except BaseException:
            traceback.print_exc()
        print(ready, file=marker_output, flush=True)
finally:
    loop.close()
