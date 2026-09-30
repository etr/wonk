fn main() {
    for (lang, src) in [
        (wonk::indexer::Lang::Python, "def f(x):\n    # 3 lines elided\n\ndef g():\n    return 1\n"),
        (wonk::indexer::Lang::Ruby, "def f(x)\n  # 3 lines elided\nend\n"),
        (wonk::indexer::Lang::Rust, "fn f() { /* 3 lines elided */ }\n"),
        (wonk::indexer::Lang::Go, "package p\n\nfunc F() { /* 3 lines elided */ }\n"),
        (wonk::indexer::Lang::Php, "<?php\nfunction f($n) { /* 3 lines elided */ }\n"),
        (wonk::indexer::Lang::CSharp, "class C {\n    int M() { /* 3 lines elided */ }\n}\n"),
    ] {
        let mut p = wonk::indexer::get_parser(lang);
        let t = p.parse(src, None).unwrap();
        println!("{:?}: has_error={}", lang, t.root_node().has_error());
    }
}
