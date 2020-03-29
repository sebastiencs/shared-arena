<h1 align="center">shared-arena</h1>
<div align="center">
  <strong>
    A thread-safe, lock-free & efficient memory pool
  </strong>
</div>


<br />

<div align="center">
  <a href="https://github.com/sebastiencs/shared-arena">
    <img src="https://img.shields.io/github/last-commit/sebastiencs/shared-arena?style=flat-square"
         alt="Last activity" />
  </a>
  <!-- Status -->
  <a href="https://github.com/sebastiencs/shared-arena">
    <img src="https://img.shields.io/badge/status-stable-orange?style=flat-square"
         alt="Status" />
  </a>
  <!-- Rust toolchain -->
  <a href="https://github.com/sebastiencs/shared-arena">
    <img src="https://img.shields.io/badge/rust-stable-blue?style=flat-square"
         alt="rust toolchain" />
  </a>
</div>

<br />

Memory pools are usefull when allocating and deallocating lots of data of the same size.  
Using a memory pool speed up those allocations/deallocations.  

This crate provides 2 memory pools:
- `SharedArena`: For multi-threaded usage
- `Pool`: For single thread only


![](https://github.com/sebastiencs/shared-arena/blob/images/table.svg)

![](https://github.com/sebastiencs/shared-arena/blob/images/bench.svg)
