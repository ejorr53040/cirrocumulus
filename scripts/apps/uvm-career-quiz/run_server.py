# Our own launcher for UVM-Career-Quiz's app.py, which defines a Flask
# `app` object but has no __main__ block of its own (it's normally run via
# `flask run`). Not part of the upstream repo -- staged into the rootfs
# alongside it by build_rootfs.sh.
from app import app

app.run(host="0.0.0.0", port=5000)
