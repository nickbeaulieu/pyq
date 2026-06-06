from app.core import exported_api


def test_exported():                # live: pytest test
    assert exported_api() == 1
