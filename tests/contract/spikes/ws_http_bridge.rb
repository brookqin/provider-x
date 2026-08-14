#!/usr/bin/env ruby
# frozen_string_literal: true

require "base64"
require "digest/sha1"
require "json"
require "net/http"
require "securerandom"
require "socket"
require "uri"
require "yaml"

LISTEN_HOST = "127.0.0.1"
LISTEN_PORT = Integer(ENV.fetch("PROVIDER_X_SPIKE_PORT", "43129"), 10)
CONFIG_PATH = File.expand_path(
  ENV.fetch(
    "PROVIDER_X_CONFIG",
    "~/Library/Application Support/dev.qiankun.provider-x/providers.yaml"
  )
)
PROVIDER_ID = ENV.fetch("PROVIDER_X_SPIKE_PROVIDER", "deepseek")
WS_GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"

def provider_config
  document = YAML.safe_load(File.binread(CONFIG_PATH), permitted_classes: [], aliases: false)
  provider = document.fetch("providers").find { |entry| entry.fetch("id") == PROVIDER_ID }
  abort("Provider not found: #{PROVIDER_ID}") unless provider
  provider
end

def read_exact(socket, length)
  result = +""
  result << socket.readpartial(length - result.bytesize) while result.bytesize < length
  result
end

def read_frame(socket)
  first, second = read_exact(socket, 2).unpack("CC")
  opcode = first & 0x0f
  masked = (second & 0x80) != 0
  length = second & 0x7f
  length = read_exact(socket, 2).unpack1("n") if length == 126
  length = read_exact(socket, 8).unpack1("Q>") if length == 127
  mask = masked ? read_exact(socket, 4).bytes : nil
  payload = read_exact(socket, length)
  if mask
    payload = payload.bytes.each_with_index.map { |byte, index| byte ^ mask[index % 4] }.pack("C*")
  end
  [opcode, payload]
end

def send_frame(socket, opcode, payload)
  payload = payload.b
  header = if payload.bytesize < 126
             [0x80 | opcode, payload.bytesize].pack("CC")
           elsif payload.bytesize <= 0xffff
             [0x80 | opcode, 126, payload.bytesize].pack("CCn")
           else
             [0x80 | opcode, 127, payload.bytesize].pack("CCQ>")
           end
  socket.write(header)
  socket.write(payload)
end

def send_json(socket, value)
  send_frame(socket, 0x1, JSON.generate(value))
end

def accept_websocket(socket)
  request = +""
  request << socket.readpartial(4096) until request.include?("\r\n\r\n")
  head = request.split("\r\n\r\n", 2).first
  lines = head.split("\r\n")
  method, path, = lines.shift.split(" ")
  headers = lines.to_h do |line|
    name, value = line.split(":", 2)
    [name.downcase, value.to_s.strip]
  end
  raise "invalid WebSocket request" unless method == "GET" && path == "/v1/responses"
  key = headers.fetch("sec-websocket-key")
  accept = Base64.strict_encode64(Digest::SHA1.digest(key + WS_GUID))
  socket.write(
    "HTTP/1.1 101 Switching Protocols\r\n" \
    "Upgrade: websocket\r\n" \
    "Connection: Upgrade\r\n" \
    "Sec-WebSocket-Accept: #{accept}\r\n\r\n"
  )
end

def warmup_events(socket, response_id)
  send_json(socket, { type: "response.created", response: { id: response_id } })
  send_json(
    socket,
    {
      type: "response.completed",
      response: {
        id: response_id,
        usage: {
          input_tokens: 0,
          input_tokens_details: nil,
          output_tokens: 0,
          output_tokens_details: nil,
          total_tokens: 0
        }
      }
    }
  )
end

def http_response(socket, provider, create, warmup, warmup_id, history, evidence)
  request = warmup_id && create["previous_response_id"] == warmup_id ? warmup.merge(create) : create.dup
  request.delete("type")
  request.delete("generate")
  request.delete("previous_response_id") if request["previous_response_id"] == warmup_id
  if request.key?("previous_response_id") && !history.empty?
    current_input = request["input"].is_a?(Array) ? request["input"] : [request["input"]]
    request["input"] = history + current_input.compact
    request.delete("previous_response_id")
    evidence << { event: "history_replayed", item_count: request["input"].length }
  end
  request["model"] = request.fetch("model").split("/", 2).last
  request["stream"] = true
  request["store"] = true

  uri = URI.join(provider.fetch("endpoints").fetch("http").sub(%r{/*$}, "/"), "responses")
  http = Net::HTTP::Proxy(:ENV).new(uri.host, uri.port)
  http.use_ssl = uri.scheme == "https"
  http.open_timeout = 10
  http.read_timeout = 300
  upstream = Net::HTTP::Post.new(uri.request_uri)
  upstream["Authorization"] = "Bearer #{provider.fetch("auth").fetch("api_key")}"
  upstream["Content-Type"] = "application/json"
  upstream["Accept"] = "text/event-stream"
  upstream.body = JSON.generate(request)

  completed = false
  response_id = nil
  response_output = []
  http.request(upstream) do |response|
    evidence << { event: "http_response", status: response.code.to_i }
    unless response.code.to_i.between?(200, 299)
      body = response.body.to_s
      parsed = JSON.parse(body) rescue { "message" => "non-JSON upstream error" }
      send_json(socket, { type: "error", status: response.code.to_i, error: parsed["error"] || parsed })
      return [nil, false]
    end

    buffer = +""
    response.read_body do |chunk|
      buffer << chunk.gsub("\r\n", "\n")
      while (boundary = buffer.index("\n\n"))
        block = buffer.slice!(0, boundary + 2)
        data = block.lines.map do |line|
          line.delete_prefix("data:").strip if line.start_with?("data:")
        end.compact.join("\n")
        next if data.empty? || data == "[DONE]"
        event = JSON.parse(data)
        response_id ||= event.dig("response", "id")
        if event["type"] == "response.completed"
          completed = true
          response_output = Array(event.dig("response", "output"))
        end
        send_frame(socket, 0x1, data)
      end
    end
  end
  evidence << { event: "http_stream", completed: completed, response_id_present: !response_id.nil? }
  request_input = request["input"].is_a?(Array) ? request["input"] : [request["input"]]
  [response_id, completed, request_input.compact + response_output]
end

provider = provider_config
server = TCPServer.new(LISTEN_HOST, LISTEN_PORT)
$stdout.sync = true
puts "ws-http bridge spike listening on #{LISTEN_HOST}:#{LISTEN_PORT}"

socket = server.accept
evidence = []
remote_response_id = nil
history = []
begin
  accept_websocket(socket)
  warmup = nil
  warmup_id = nil

  loop do
    opcode, payload = read_frame(socket)
    case opcode
    when 0x1
      create = JSON.parse(payload)
      raise "unexpected client event" unless create["type"] == "response.create"
      evidence << {
        event: "response_create",
        generate: create["generate"] != false,
        previous_response_id_present: create.key?("previous_response_id"),
        model: create.fetch("model")
      }
      if create["generate"] == false
        warmup = create
        warmup_id = "resp_provider_x_warmup_#{SecureRandom.hex(12)}"
        warmup_events(socket, warmup_id)
        evidence << { event: "warmup_simulated" }
      else
        remote_response_id, completed, history = http_response(
          socket, provider, create, warmup || {}, warmup_id, history, evidence
        )
        raise "upstream stream did not complete" unless completed
        warmup = nil
        warmup_id = nil
      end
    when 0x8
      send_frame(socket, 0x8, payload)
      break
    when 0x9
      send_frame(socket, 0xA, payload)
    when 0xA
      next
    else
      raise "unsupported WebSocket opcode #{opcode}"
    end
  end
rescue EOFError, Errno::ECONNRESET
  evidence << { event: "client_disconnected", response_id_present: !remote_response_id.nil? }
ensure
  evidence_path = ENV["PROVIDER_X_SPIKE_EVIDENCE"]
  if evidence_path
    File.open(evidence_path, File::WRONLY | File::CREAT | File::EXCL, 0o600) do |file|
      evidence.each { |entry| file.puts(JSON.generate(entry)) }
    end
  end
  socket&.close
  server&.close
end
