# Iterator chains against hand-written loops

`chains.rock` pairs each iterator chain (`chain_*`) with the loop it should
compile to (`loop_*`). The driver checks each pair agrees, then reports
ns/element (min and median over interleaved rounds) and bytes allocated per
call with the GC stopped.

```sh
rock compile benchmarks/iterator_chains/chains.rock --output /tmp/chains.lua
luajit benchmarks/iterator_chains/driver.lua /tmp/chains.lua 7
N=8 luajit benchmarks/iterator_chains/driver.lua /tmp/chains.lua 7
```

`N` sets the Vec length (default 1000); short Vecs expose per-call costs.
