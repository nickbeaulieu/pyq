__all__ = ["exported_api"]


def exported_api():        # live: in __all__
    return _helper()


def _helper():             # live: reached from exported_api
    return 1


def truly_dead():          # DEAD: nothing reaches it
    return orphan_helper()


def orphan_helper():       # DEAD: only reached from truly_dead (also dead)
    return 2


def used_by_main():        # live: called at module scope under __main__
    return 3


def main():                # live: called under __main__
    return used_by_main()


if __name__ == "__main__":
    main()


def string_referenced():   # live ONLY via the dotted-string path in registry.py
    return 99
