def c():
    return 1

def b():
    return c()

def a():
    return b()

def recur(n):
    if n <= 0:
        return 0
    return recur(n - 1)
