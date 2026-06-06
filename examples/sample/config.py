import os

DEBUG = os.getenv("DEBUG")
DB_URL = os.environ["DATABASE_URL"]
TIMEOUT = os.environ.get("TIMEOUT", "30")
key = "SECRET_" + "KEY"
secret = os.getenv(key)

def load():
    with open("settings.ini") as f:
        return f.read()
