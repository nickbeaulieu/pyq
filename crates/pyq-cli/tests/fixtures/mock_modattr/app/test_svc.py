from unittest.mock import patch


@patch("app.svc.time.sleep")
def test_valid(m): ...


@patch("app.svc.time.slep")
def test_typo(m): ...


@patch("app.svc.getcwd.anything")
def test_symbol_binding(m): ...
