# fnr – find and replace

Like `find ... | xargs sed ...`, but with a memorable interface.

Recursively find and replace patterns in files and directories.

```
fnr [OPTIONS] FIND REPLACE [PATH...]
```

## About

`fnr` is intended to be more intuitive to use than `sed`, but is not a
drop in replacement. Instead, it's focused on making bulk changes in a
directory, and is more comparable with your IDE or editor's find and
replace tool.

**fnr is alpha quality.** Don't use `--write` in situations you
wouldn't be able to revert.

## Examples

Replace `"old_function"` with `"new_function"` in current directory.
```
fnr old_function new_function
```

Choose files and directories to consider.
```
fnr 'EDITOR=vim' 'EDITOR=emacs' ~/.zshrc ~/.config/
```

We can use `--literal` so the pattern isn't treated as a regular expression.
```
fnr --literal 'i += 1' 'i++'
```

Replace using capturing groups.
```
fnr 'const (\w+) = \d+;' 'const $1 = 42;'
```

Use `-W --write` to write changes back to files.
```
fnr --write 'Linus Torvalds' 'Linux Torvalds'
```

Use `-I --include` to only modify files or directories matching a pattern.
```
fnr --include 'Test.*\.kt' 'mockito' 'mockk'
```

Similarly, use `-E --exclude` to ignore certain files.
```
fnr --exclude ChangeLog 2021 2022
```

Files and directories to consider can also be given over standard input.
```
find /tmp/ -name "*.csv" -print | fnr "," "\t"
```

Use `-p --prompt` to individually accept or reject each replacement.
```
fnr --prompt --literal 'i++' '++i'
--- ./README.md: 2 matching lines
-   18: $ fnr --literal 'i += 1' 'i++'
+   18: $ fnr --literal 'i += 1' '++i'
Stage this replacement [y,n,q,a,e,d,?] ?
```

## Installation

```
cargo install fnr
```

If you'd prefer to build from source instead:

``` console
$ git clone git@github.com/erik/fnr.git
$ cd fnr
$ cargo install --path .
```

## Performance

Built on top of [ripgrep]'s path traversal and pattern matching, so
even though performance isn't an explicit goal, it's not going to be
the bottleneck.

In fact, without writing changes back to files, it's imperceptibly
slower than ripgrep itself.

| Command                                      |        Mean [ms] | Min [ms] | Max [ms] |      Relative |
|:---------------------------------------------|-----------------:|---------:|---------:|--------------:|
| `rg "EINVAL" ./linux`                        |     510.4 ± 25.8 |    467.6 |    555.0 |          1.00 |
| `fnr "EINVAL" "ERR_INVALID" ./linux`         |     620.4 ± 22.1 |    573.7 |    649.7 |   1.22 ± 0.08 |
| `fnr --write "EINVAL" "ERR_INVALID" ./linux` |    3629.0 ± 76.2 |   3538.0 |   3802.3 |   7.11 ± 0.39 |
| `ag "EINVAL" ./linux`                        |    2560.0 ± 43.6 |   2518.4 |   2668.1 |   5.02 ± 0.27 |
| `grep -irI "EINVAL" ./linux`                 | 37215.8 ± 7444.7 |  31316.1 |  49096.6 | 72.92 ± 15.04 |

[ripgrep]: https://github.com/BurntSushi/ripgrep

## Similar Tools

If `fnr` doesn't quite fit what you're looking for, also consider:

- [facebookincubator/fastmod](https://github.com/facebookincubator/fastmod/) - quite similar to `fnr`.
- [google/rerast](https://github.com/google/rerast) - operates on Rust AST
- [coccinelle](https://coccinelle.gitlabpages.inria.fr/website/) - more advanced edits to C code
- ... many more
