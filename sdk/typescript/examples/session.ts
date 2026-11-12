/**
 * A session keeps variables, imports and files under /data between runs, like
 * cells in a notebook.
 *
 *     ERYX_API_KEY=eryx_... node examples/session.ts
 */

import { PythonExceptionError, Session } from "@eryx-org/eryx-run";

const session = new Session({ ttlSecs: 600 });
try {
  await session.execute(`
import statistics
scores = [82, 91, 77, 68, 95]
with open("/data/notes.txt", "w") as f:
    f.write("first cell ran\\n")
`);

  const second = await session.execute(`
print(statistics.mean(scores))
print(open("/data/notes.txt").read(), end="")
`);
  console.log(second.stdout.trimEnd());

  // A failed run throws, but keeps what it changed before it raised
  try {
    await session.execute("scores.append(100)\n1 / 0");
  } catch (e) {
    if (!(e instanceof PythonExceptionError)) throw e;
    console.log("raised:", e.stderr.trimEnd().split("\n").at(-1));
  }
  console.log((await session.execute("print(len(scores))")).stdout.trimEnd()); // 6

  console.log(`session ${session.id}, ${session.executionCount} runs`);
} finally {
  await session.delete();
}
