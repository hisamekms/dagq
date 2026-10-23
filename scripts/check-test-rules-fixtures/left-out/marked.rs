// test-rules: allow A-137 the test checks that the runtime's own loop stops
const A: &str = "while [ ! -f go ]; do sleep 1; done";
const B: &str = "until [ -e go ]; do sleep 1; done"; // test-rules: allow A-137 a stub of the shape under test
const C: &str = "await_file() { while [ ! -f \"$1\" ]; do sleep 0.05; done; }";
// a comment: while [ ! -f go ]; do sleep 1; done
const D: &str = "while [ ! -e stop ]; do :; done; sleep 1";
const E: &str = "while [ ! -f go ]; do
    sleep 1
done";
const L: &str = "until [ -f go ]; do sleep 1; done"; /* test-rules: allow A-137 a stub of the shape under test */
const P: &str = "while [ ! -e sleep.stop ]; do [ -d tmp ] && kill -0 123 || exit 1; done";
const Q: &str = "while [ ! -f go ]; do cat \"$dir/sleep\"; done";
const R: &str = "until [ -e go ]; do echo sleep; done";
