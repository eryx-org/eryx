// A stand-in for a jco-transpiled command whose `wasi:cli/run` returns its
// error (as a C `main` returning nonzero does): jco throws a ComponentError,
// an Error with the error value as an own `payload`.
export async function instantiate() {
  return {
    run: {
      run() {
        const error = new Error("run returned an error");
        Object.defineProperty(error, "payload", { value: undefined });
        throw error;
      },
    },
  };
}
