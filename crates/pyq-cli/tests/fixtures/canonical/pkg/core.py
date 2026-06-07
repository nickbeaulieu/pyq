# `normalize` is the repo's blessed helper: three production callers reach for
# it. The `parse_*` functions wrap it; `tested_public` is exercised by a test,
# the rest of the public surface is not.


def normalize(x):
    return x.strip().lower()


def parse_name(s):
    return normalize(s)


def parse_title(s):
    return normalize(s)


def parse_tag(s):
    return normalize(s)


def tested_public(s):
    return parse_name(s)


def untested_public(s):
    return parse_title(s)


def _private(s):
    return s
