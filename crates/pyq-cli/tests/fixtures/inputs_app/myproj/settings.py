# The app's config surface: all reads go through a wrapper, plus one direct
# stdlib read. None of these are scripts — this is what `pyq inputs` shows by
# default.
import os

var_provider = EnvVarProvider()

DB_PASSWORD = var_provider.get_var("DB_PASSWORD")
SECRET_KEY = var_provider.get_var("SECRET_KEY")
STRIPE_KEY = var_provider.get_var("STRIPE_SECRET_KEY")
DEBUG = os.getenv("DEBUG", "false")
