import os
import subprocess
import glob

def generate_protos():

    proto_files = glob.glob("proto/**/*.proto", recursive=True)

    for ext in ("*_pb2.py", "*_pb2.pyi"):
        for file in glob.glob(f"proto/**/{ext}", recursive=True):
            os.remove(file)

    for proto_file in proto_files:
        rel_path = os.path.dirname(proto_file)
        rel_path = rel_path[len("proto/"):]
        print(proto_file.split('/')[-1])

        cmd = [
            "python",
            "-m",
            "grpc_tools.protoc",
            "-I.",
            f"--python_out=.",
            f"--mypy_out=.",
            f'--grpc_python_out=.',
            proto_file
        ]
        subprocess.run(cmd, check=True)

    print("Proto files generated successfully!")

if __name__ == "__main__":
    generate_protos()