const A: &str = "x";
const B: &str = "while [ ! -f go ]; do sleep 1; done";
const C: &str = "while [ ! -e go ]; do sleep 1; done";
const D: &str = "until [ -f go ]; do sleep 1; done";
const E: &str = "until [ -e go ]; do sleep 1; done";
// a mark needs a number and a reason
const F: &str = "x"; // test-rules: allow A-137
// test-rules: allow A-262 a mark for another rule does not take the line out
const G: &str = "until [ -f go ]; do sleep 1; done";
const H: &str = "while [ ! -f \"$dir/done\" ]; do sleep 1; done";
const I: &str = "while [ ! -f stop ]; do :; done; while [ ! -f go ]; do sleep 1; done";
const J: &str = "until [ -e go ]; do sleep 1; done"; /* test-rules: allow A-137 */
/* test-rules: allow A-137 */
const K: &str = "while [ ! -f go ]; do sleep 1; done";
const M: &str = "until [ -f a ]; do sleep 1; done"; // test-rules: allow A-137 a stub of the shape under test
const N: &str = "until [ -f b ]; do sleep 1; done";
const O: &str = "while [ ! -f go ]; do kill -0 1 && sleep 1; done";
