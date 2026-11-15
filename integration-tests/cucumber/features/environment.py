import subprocess
import time
import requests
import os
import signal

def start_server(context, args=None):
    # Path to the server executable
    project_root = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", "..", ".."))
    server_bin = os.path.join(project_root, "target", "debug", "graphbuff-server.exe")
    
    # Start the server on a random port (or a fixed one for simplicity in tests)
    # Using a fixed port for now, but port 0 would be safer for parallel tests
    if not hasattr(context, 'server_port'):
        context.server_port = 7475
    context.base_url = f"http://127.0.0.1:{context.server_port}"
    
    cmd = [server_bin, "--bind", f"127.0.0.1:{context.server_port}", "--in-memory"]
    if args:
        cmd.extend(args)
    
    print(f"Starting server: {' '.join(cmd)}")
    server_process = subprocess.Popen(
        cmd,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True
    )
    context.server_process = server_process
    
    # Wait for the server to be ready
    max_retries = 20
    for i in range(max_retries):
        try:
            response = requests.get(f"{context.base_url}/health", timeout=1)
            if response.status_code == 200:
                print("Server is ready")
                return
        except requests.exceptions.ConnectionError:
            pass
        time.sleep(0.2)
    
    # If we get here, server didn't start
    context.server_process.terminate()
    stdout, stderr = context.server_process.communicate()
    raise RuntimeError(f"Server failed to start. STDOUT: {stdout}\nSTDERR: {stderr}")

def stop_server(context):
    if hasattr(context, 'server_process'):
        context.server_process.terminate()
        context.server_process.wait()
        # We don't delete it, just set it to None to avoid KeyError in behave's __setattr__
        context.server_process = None

def before_all(context):
    start_server(context)

def before_scenario(context, scenario):
    # For scenarios that don't explicitly start their own server, 
    # we might want a fresh one, but restarting for EVERY scenario is slow.
    # However, for correctness, let's ensure we are in a known state.
    # If the server is already running in memory, we can just clear it or restart it.
    if not hasattr(context, 'data_dir'):
        stop_server(context)
        start_server(context)

def after_all(context):
    stop_server(context)
