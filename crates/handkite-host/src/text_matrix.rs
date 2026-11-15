//! Text classes that direct injection must carry byte-for-byte.
//!
//! Every non-ASCII scalar is written as an escape so this file stays ASCII and
//! the exact sequence under test is visible: precomposed and decomposed forms,
//! joiners, variation selectors, and modifiers are easy to confuse otherwise.

/// `(class, text)`; each text is a single line and must be accepted whole.
pub(crate) const TEXT_MATRIX: &[(&str, &str)] = &[
    (
        "simplified_chinese",
        "\u{4f60}\u{597d}\u{ff0c}\u{4e16}\u{754c}\u{3002}\u{7b80}\u{4f53}\u{4e2d}\u{6587}\u{8f93}\u{5165}\u{6d4b}\u{8bd5}",
    ),
    (
        "traditional_chinese",
        "\u{7e41}\u{9ad4}\u{4e2d}\u{6587}\u{8f38}\u{5165}\u{6e2c}\u{8a66}\u{ff0c}\u{81fa}\u{7063}\u{8207}\u{9999}\u{6e2f}",
    ),
    (
        "supplementary_plane_han",
        "\u{20bb7}\u{91ce}\u{5bb6}\u{3068}\u{29e3d}",
    ),
    (
        "japanese_kanji_hiragana_katakana",
        "\u{65e5}\u{672c}\u{8a9e}\u{306e}\u{6f22}\u{5b57}\u{3068}\u{3072}\u{3089}\u{304c}\u{306a}\u{3068}\u{30ab}\u{30bf}\u{30ab}\u{30ca}",
    ),
    (
        "japanese_dakuten_composed",
        "\u{304c}\u{304e}\u{3050}\u{3052}\u{3054} \u{30d1}\u{30d4}\u{30d7}\u{30da}\u{30dd}",
    ),
    (
        "japanese_dakuten_decomposed",
        "\u{304b}\u{3099}\u{304d}\u{3099} \u{30cf}\u{309a}\u{30d2}\u{309a}",
    ),
    (
        "japanese_halfwidth_katakana",
        "\u{ff76}\u{ff9e}\u{ff77}\u{ff9e}\u{ff78}\u{ff9e} \u{ff8a}\u{ff9f}",
    ),
    (
        "fullwidth_punctuation",
        "\u{ff08}\u{5168}\u{89d2}\u{ff09}\u{ff01}\u{ff1f}\u{300c}\u{300d}\u{3001}\u{3002}",
    ),
    (
        "korean_syllables",
        "\u{d55c}\u{ad6d}\u{c5b4} \u{c785}\u{b825} \u{d14c}\u{c2a4}\u{d2b8}",
    ),
    (
        "korean_conjoining_jamo",
        "\u{1112}\u{1161}\u{11ab}\u{1100}\u{1173}\u{11af}",
    ),
    (
        "arabic",
        "\u{645}\u{631}\u{62d}\u{628}\u{627} \u{628}\u{627}\u{644}\u{639}\u{627}\u{644}\u{645}",
    ),
    (
        "persian_zwnj",
        "\u{645}\u{6cc}\u{200c}\u{62e}\u{648}\u{627}\u{647}\u{645}",
    ),
    (
        "hebrew",
        "\u{5e9}\u{5dc}\u{5d5}\u{5dd} \u{5e2}\u{5d5}\u{5dc}\u{5dd}",
    ),
    (
        "bidi_mixed",
        "abc \u{645}\u{631}\u{62d}\u{628}\u{627} 123 \u{5e9}\u{5dc}\u{5d5}\u{5dd} xyz",
    ),
    (
        "thai",
        "\u{e2a}\u{e27}\u{e31}\u{e2a}\u{e14}\u{e35}\u{e04}\u{e23}\u{e31}\u{e1a} \u{e20}\u{e32}\u{e29}\u{e32}\u{e44}\u{e17}\u{e22} \u{e19}\u{e49}\u{e33}",
    ),
    (
        "devanagari_conjuncts",
        "\u{915}\u{94d}\u{937}\u{924}\u{94d}\u{930}\u{93f}\u{92f} \u{939}\u{93f}\u{928}\u{94d}\u{926}\u{940}",
    ),
    (
        "accented_latin_composed",
        "Caf\u{e9} na\u{ef}ve Z\u{fc}rich \u{c5}ngstr\u{f6}m se\u{f1}or \u{110}\u{e0} N\u{1eb5}ng",
    ),
    (
        "accented_latin_decomposed",
        "Cafe\u{301} nai\u{308}ve A\u{30a}ngstro\u{308}m",
    ),
    ("single_emoji", "\u{1f600}"),
    (
        "emoji_variation_selector",
        "\u{2764}\u{fe0f} \u{2600}\u{fe0f}",
    ),
    ("emoji_keycap", "1\u{fe0f}\u{20e3}#\u{fe0f}\u{20e3}"),
    (
        "emoji_zwj_family",
        "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}\u{200d}\u{1f466}",
    ),
    ("emoji_skin_tone", "\u{1f44b}\u{1f3fd}\u{1f44d}\u{1f3ff}"),
    ("emoji_zwj_skin_tone", "\u{1f9d1}\u{1f3ff}\u{200d}\u{1f4bb}"),
    (
        "emoji_flags",
        "\u{1f1ef}\u{1f1f5}\u{1f1f9}\u{1f1fc}\u{1f1fa}\u{1f1f8}",
    ),
    (
        "emoji_subdivision_flag",
        "\u{1f3f4}\u{e0067}\u{e0062}\u{e0073}\u{e0063}\u{e0074}\u{e007f}",
    ),
    (
        "mixed_english_cjk_emoji",
        "Hello \u{4f60}\u{597d} \u{3053}\u{3093}\u{306b}\u{3061}\u{306f} \u{c548}\u{b155} \u{1f44b}\u{1f3fd} OK",
    ),
    (
        "ascii_rewrite_bait",
        "\"quote\" 'single' -- ... a  b i teh (c) 1/2",
    ),
];

/// About 500 scalars of mixed scripts for rate and limit checks.
pub(crate) fn long_mixed_text() -> String {
    let unit = TEXT_MATRIX
        .iter()
        .find(|(class, _)| *class == "mixed_english_cjk_emoji")
        .expect("mixed class")
        .1;
    let mut text = String::new();
    while text.chars().count() < 500 {
        text.push_str(unit);
        text.push(' ');
    }
    text
}
