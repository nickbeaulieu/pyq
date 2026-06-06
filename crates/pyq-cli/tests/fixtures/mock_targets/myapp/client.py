import requests

API = "https://example.com"

class Client:
    def fetch(self):
        return requests.get(API)

# A subclass with a base: members it doesn't declare may be inherited, so a
# miss on it is unverifiable, not drift.
class Account(dict):
    def balance(self):
        return 0

def helper():
    return 1
