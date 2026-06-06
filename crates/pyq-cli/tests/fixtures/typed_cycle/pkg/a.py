from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from pkg import b


def use():
    from pkg import b
    return b.thing
