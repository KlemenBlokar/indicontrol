# OpenNGC

`NGC.csv` and `addendum.csv` are the unmodified database files of
[OpenNGC](https://github.com/mattiaverga/OpenNGC) release v20260501
by Mattia Verga, licensed under CC-BY-SA-4.0 (see `LICENSE`). The engine
embeds them for object lookup (`src/catalog.rs`).

The rest of indicontrol is under the WTFPL; these data files keep their own
license.

To update, replace both files with those from a newer release's
`database_files/` and run `cargo test -p engine catalog`.
