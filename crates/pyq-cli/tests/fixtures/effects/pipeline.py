from io_ops import fetch, save, jitter

def run(url):
    data = fetch(url)
    save(data)
    jitter()
    return data

def pure_add(a, b):
    return a + b
