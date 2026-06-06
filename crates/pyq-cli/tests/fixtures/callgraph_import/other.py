# A second, unrelated `helper` with the same name — its caller must NOT show up
# in lib.helper's reverse closure.
def helper():
    return 2

def use_other():
    return helper()
