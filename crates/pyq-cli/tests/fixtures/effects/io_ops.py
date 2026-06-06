import requests
import sqlite3
import random

def fetch(url):
    return requests.get(url)

def save(row):
    conn = sqlite3.connect("db.sqlite")
    conn.execute("insert into t values (?)", [row])
    return conn

def jitter():
    return random.random()

CACHE = {}

def remember(k, v):
    global CACHE
    CACHE[k] = v
