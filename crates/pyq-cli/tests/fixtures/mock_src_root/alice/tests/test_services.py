from unittest.mock import patch


@patch("main.services.get_thing")
def test_ok(m): ...


@patch("main.services.removed_thing")
def test_drifted(m): ...
