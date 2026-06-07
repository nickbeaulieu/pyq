from models import reverse_choices as rc


class Settings:
    # A class-body call through an aliased import — runs at class definition
    # (import) time. ty's def-anchored incoming_calls misses this aliased,
    # module-scope caller; the recording recovers it.
    DEFAULT = rc("default")
