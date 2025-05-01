package main

import (
	"bufio"
	"encoding/json"
	"fmt"
	"log"
	"net"
	"os"
	"os/exec"
	"regexp"
	"strconv"
	"strings"
	"sync"
)

// TermInfo represents terminal metadata
type TermInfo struct {
	Cols    int    `json:"cols"`
	Rows    int    `json:"rows"`
	Type    string `json:"type"`
	Version string `json:"version"`
	Theme   map[string]interface{} `json:"theme"`
}

// CastHeader represents the header of an asciinema cast file
type CastHeader struct {
	Version   int      `json:"version"`
	Term      TermInfo `json:"term"`
	Timestamp int64    `json:"timestamp"`
	Env       map[string]string `json:"env"`
	ChildPID  int        `json:"child_pid"`
}

// CastEvent represents an event in an asciinema cast file
type CastEvent struct {
	Time   float64 `json:"time,omitempty"`
	Type   string  `json:"type,omitempty"`
	Data   string  `json:"data,omitempty"`
	PID    int     `json:"pid,omitempty"`
}

// CommandBuffer holds lines for a command session
type CommandBuffer struct {
	Lines []string
	Active bool
}

type SessionState string

const (
	StateIdle    SessionState = "Idle"
	StatePrompt  SessionState = "Prompt"
	StateCommand SessionState = "Command"
)

type TerminalSession struct {
	PID           int
	State         SessionState
	CommandBuffer []string
	PromptBuffer  []string
	LastExitCode  int
	CommandString string
	CurrentInput  string

	// Claude Code state
	ClaudeActive  bool
	ClaudeCommand string
	ClaudeState   SessionState
	ClaudeNotifiedInsert bool
}

var (
	oscDRegexp = regexp.MustCompile(`\x1b]133;D;(\d+)\x07`)
	oscPattern = regexp.MustCompile(`^\x1b\](133;[CD]|1337;RemoteHost=|1337;CurrentDir=)`)
	ansiRegexp = regexp.MustCompile(`\x1b\[[0-9;?]*[A-Za-z]|\x1b\][^\a]*\a|\x1b\][^\x07]*\x07|\x1b\][^\x1b]*\x1b\\`)
)

func notify(message string) {
	// Create a more advanced AppleScript
	script := `
	on run argv
			set message to item 1 of argv
			display notification message with title "✓ Claude Code - Next Step Ready " sound name "Glass"
	end run
	`

	// Execute the AppleScript with the message as an argument
	cmd := exec.Command("osascript", "-e", script, message)
	err := cmd.Run()
	if err != nil {
			log.Printf("Error executing AppleScript: %v", err)
	}
}


func looksLikeJSON(s string) bool {
	s = strings.TrimSpace(s)
	if len(s) < 2 {
		return false
	}
	return (s[0] == '{' && s[len(s)-1] == '}') || (s[0] == '[' && s[len(s)-1] == ']')
}

func extractExitCode(data string) int {
	matches := oscDRegexp.FindStringSubmatch(data)
	if len(matches) == 2 {
		if code, err := strconv.Atoi(matches[1]); err == nil {
			return code
		}
	}
	// If not found or error, return -1 (should not happen if always present)
	return -1
}

// Add this function for extracting the command from OSC 133;B
func extractCommandFromOSC133B(line string) string {
	start := strings.Index(line, "\x1b]133;B\a")
	if start == -1 {
		return ""
	}
	afterB := line[start+len("\x1b]133;B\a"):]
	end := strings.Index(afterB, "\x1b[K")
	if end != -1 {
		afterB = afterB[:end]
	}
	return strings.TrimSpace(afterB)
}

// Add a helper to check if a line is real output (not just OSC/control)
func isRealOutput(data string) bool {
	// Skip OSC 133;C, 133;D, 1337;RemoteHost, 1337;CurrentDir, etc
	return !oscPattern.MatchString(data) && strings.TrimSpace(data) != ""
}

func stripANSI(input string) string {
	return ansiRegexp.ReplaceAllString(input, "")
}

func removeBoxDrawingChars(s string) string {
	boxChars := []rune{'╭', '╮', '╯', '╰', '│', '─'}
	replacer := s
	for _, c := range boxChars {
		replacer = strings.ReplaceAll(replacer, string(c), "")
	}
	return replacer
}

func handleClaudeSession(session *TerminalSession, data string) {
	clean := stripANSI(data)
	clean = removeBoxDrawingChars(clean)
	clean = strings.ReplaceAll(clean, "\r", "")
	clean = strings.ReplaceAll(clean, "\n", "")

	// if strings.Contains(clean, "Do you want to proceed?") {
		// fmt.Println("[CLAUDE PROMPT] Do you want to proceed?")
		// notify("Decision: Do you want to proceed?")
	// }

	if strings.Contains(clean, "⏺ ") {
		fmt.Println("[CLAUDE PROMPT] Insert mode")
		if !session.ClaudeNotifiedInsert && session.ClaudeCommand != "" {
			notify(fmt.Sprintf("%s", session.ClaudeCommand))
			session.ClaudeNotifiedInsert = true
		}
	}

	// fmt.Printf("[claude] %q\n", clean)

	// Detect command start: look for promptline like '> search hi' (after stripping)
	if strings.HasPrefix(clean, "> ") {
		cmd := strings.TrimSpace(strings.TrimPrefix(clean, "> "))
		// trim anything after "·", which may not present
		if strings.Contains(cmd, "·") {
			cmd = strings.Split(cmd, "·")[0]
		}
		if session.ClaudeState != StateCommand || session.ClaudeCommand != cmd {
			if session.ClaudeState == StateCommand && session.ClaudeCommand != "" {
				fmt.Printf("[CLAUDE CMD END >] %s\n", session.ClaudeCommand)
			}
			fmt.Printf("[CLAUDE CMD START] %s\n", cmd)
			session.ClaudeState = StateCommand
			session.ClaudeCommand = cmd
			session.ClaudeActive = true
			session.ClaudeNotifiedInsert = false
		}
		return // Don't print the prompt line itself
	}

	// Detect command end: look for a line like '-- INSERT --' or empty line after output
	if strings.Contains(clean, "8;2;136;136;136m  -- INSERT --") {
		if session.ClaudeState == StateCommand && session.ClaudeCommand != "" {
			fmt.Printf("[CLAUDE CMD END I] %s\n", session.ClaudeCommand)
			if !session.ClaudeNotifiedInsert && session.ClaudeCommand != "" {
				notify(fmt.Sprintf("%s", session.ClaudeCommand))
				session.ClaudeNotifiedInsert = true
			}
			
			session.ClaudeState = StatePrompt
			// notify(fmt.Sprintf("Command Ended: %s", session.ClaudeCommand))
			session.ClaudeCommand =""
			session.ClaudeActive = false
			session.ClaudeNotifiedInsert = false
		}
		return
	}

	// Print output lines only if in Claude command state
	if session.ClaudeState == StateCommand && clean != "" {
		fmt.Println(clean)
	}
}

func main() {
	socketPath := "/tmp/test.sock"
	
	// Remove socket if it already exists
	if err := os.RemoveAll(socketPath); err != nil {
		log.Fatal("Error removing existing socket:", err)
	}
	
	// Create the socket
	listener, err := net.Listen("unix", socketPath)
	if err != nil {
		log.Fatal("Error creating socket:", err)
	}
	defer listener.Close()
	
	fmt.Printf("2Unix socket server listening on %s\n", socketPath)
	
	// WaitGroup to track active connections
	var wg sync.WaitGroup
	
	// Track terminal info by connection
	terminalInfoMutex := &sync.Mutex{}
	terminalInfo := make(map[net.Conn]*CastHeader)
	
	
	for {
		// Accept a connection
		conn, err := listener.Accept()
		if err != nil {
			log.Printf("Error accepting connection: %v", err)
			continue
		}
		
		// Launch a goroutine to handle this connection
		wg.Add(1)
		go handleConnection(conn, &wg, terminalInfo, terminalInfoMutex)
	}
}

func handleConnection(conn net.Conn, wg *sync.WaitGroup, terminalInfo map[net.Conn]*CastHeader, mutex *sync.Mutex) {
	defer func() {
		conn.Close()
		
		// Remove terminal info when connection closes
		mutex.Lock()
		delete(terminalInfo, conn)
		mutex.Unlock()
		
		wg.Done()
	}()
	
	// Read data from the connection
	scanner := bufio.NewScanner(conn)
	
	// Generate a unique ID for this connection
	connID := fmt.Sprintf("%p", conn)
	fmt.Printf("New connection established: %s\n", connID)
	
	sessions := make(map[int]*TerminalSession)
	
	for scanner.Scan() {
		line := scanner.Text()
		trimmed := strings.TrimSpace(line)
		if looksLikeJSON(trimmed) && trimmed[0] == '[' {
			var arr []interface{}
			err := json.Unmarshal([]byte(trimmed), &arr)
			if err != nil || len(arr) < 4 {
				fmt.Printf("[unknown] %s\n", trimmed)
				continue
			}
			pid, okPid := arr[3].(float64)
			data, okData := arr[2].(string)
			if !okPid || !okData {
				fmt.Printf("[unknown] %s\n", trimmed)
				continue
			}
			pidInt := int(pid)
			session, exists := sessions[pidInt]
			if !exists {
				session = &TerminalSession{PID: pidInt, State: StateIdle}
				sessions[pidInt] = session
			}

			// [raw <pid>] logging

			cleaner := removeBoxDrawingChars(data)
			//cleaner = stripANSI(cleaner)
			//cleaner = strings.ReplaceAll(cleaner, "\r", "")
			//cleaner = strings.ReplaceAll(cleaner, "\n", "")

			fmt.Printf("[raw %d] %q\n", pidInt, cleaner)

			switch {
			case strings.Contains(data, "\x1b]133;B\a"):
				cmd := extractCommandFromOSC133B(data)
				if cmd != "" {
					fmt.Printf("[COMMAND] Just entered: %q\n", cmd)
					session.CommandString = cmd
					session.State = StateCommand // Set state to Command
					session.CommandBuffer = nil  // Clear previous buffer
				}
			case session.CommandString == "claude":
				// fmt.Printf("[?CLAUDE] %q\n", data)
				handleClaudeSession(session, data)
			case strings.Contains(data, "\x1b]133;D"):
				// fmt.Printf("[debug] OSC 133;D: CommandBuffer=%v\n", session.CommandBuffer)
				session.State = StatePrompt
				// Do not append OSC 133;D to CommandBuffer, just handle exit code
				exitCode := extractExitCode(data)
				session.LastExitCode = exitCode
				// Print command, exit code, and output directly
				fmt.Printf("[COMMAND END] PID %d, exit=%d\n", session.PID, session.LastExitCode)
				fmt.Printf("  Command: %q\n", session.CommandString)
				for _, l := range session.CommandBuffer {
					fmt.Printf("    %q\n", l)
				}
				fmt.Println("---")
				session.CommandBuffer = nil
				session.CommandString = ""
				session.CurrentInput = ""
			default:
				if session.State == StateCommand {
					if isRealOutput(data) {
						session.CommandBuffer = append(session.CommandBuffer, data)
					}
				} else if session.State == StatePrompt {
					session.PromptBuffer = append(session.PromptBuffer, data)
				}
			}
		} else {
			fmt.Printf("[unknown] %s\n", trimmed)
		}
	}
	
	if err := scanner.Err(); err != nil {
		log.Printf("Error reading from connection: %v", err)
	}
}