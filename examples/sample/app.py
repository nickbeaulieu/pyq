from pkg.models import User, make_user

def main():
    u = make_user("ada")
    admin = User("root")
    print(u.name, admin.name)

if __name__ == "__main__":
    main()
